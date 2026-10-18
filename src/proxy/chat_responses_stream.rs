//! The bounded Chat -> Responses SSE converter only. Source tool indices,
//! reserved output order, and published item lifecycles are separate state.

use super::{AppState, Response, convert, responses_event, sse_response};
use async_stream::stream;
use axum::body::Body;
use bytes::Bytes;
use futures_util::StreamExt;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;

const PENDING_LIMIT: usize = 1024 * 1024;
type StreamResult<T> = Result<T, &'static str>;

/// A frame is decoded only after whole lines have arrived. In particular,
/// neither UTF-8 code points nor CRLF pairs need align with HTTP chunks.
#[derive(Default)]
struct DataFrames {
    line: Vec<u8>,
    data: Option<String>,
    skip_lf: bool,
    // A blank CR line cannot dispatch/reset until its possible LF is charged.
    pending_frame: bool,
    started: bool,
    frame_bytes: usize,
}

impl DataFrames {
    fn byte(&mut self, byte: u8, limit: usize) -> StreamResult<Option<String>> {
        let mut completed = None;
        if std::mem::take(&mut self.skip_lf) {
            if byte == b'\n' {
                self.charge(limit)?;
                return Ok(self.finish_cr());
            }
            completed = self.finish_cr();
        }
        self.charge(limit)?;
        if byte != b'\n' && byte != b'\r' {
            self.line.push(byte);
            return Ok(completed);
        }
        self.skip_lf = byte == b'\r';
        let line = String::from_utf8(std::mem::take(&mut self.line))
            .map_err(|_| "Invalid upstream SSE UTF-8")?;
        let line = if std::mem::replace(&mut self.started, true) {
            line.as_str()
        } else {
            line.strip_prefix('\u{feff}').unwrap_or(&line)
        };
        if line.is_empty() {
            if self.skip_lf {
                self.pending_frame = true;
                return Ok(completed);
            }
            self.frame_bytes = 0;
            return Ok(self.data.take());
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            }
        }
        Ok(completed)
    }

    fn charge(&mut self, limit: usize) -> StreamResult<()> {
        self.frame_bytes = checked_budget(self.frame_bytes, 1, limit, "SSE frame limit exceeded")?;
        Ok(())
    }

    // A non-LF lookahead or end of transport resolves a standalone CR.
    fn finish_cr(&mut self) -> Option<String> {
        if std::mem::take(&mut self.pending_frame) {
            self.frame_bytes = 0;
            self.data.take()
        } else {
            None
        }
    }

    fn eof(&mut self) -> StreamResult<Option<String>> {
        let completed = self.finish_cr();
        if !self.line.is_empty() || self.data.is_some() {
            return Err("Unfinished upstream SSE frame");
        }
        Ok(completed)
    }
}

fn checked_budget(
    current: usize,
    extra: usize,
    limit: usize,
    error: &'static str,
) -> StreamResult<usize> {
    current
        .checked_add(extra)
        .filter(|size| *size <= limit)
        .ok_or(error)
}

#[derive(Default)]
struct Tool {
    call_id: Option<Box<str>>,
    name: Option<Box<str>>,
    kind: Option<Box<str>>,
    saw_arguments: bool,
}

impl Tool {
    fn ready(&self) -> bool {
        self.call_id.is_some() && self.name.is_some() && self.kind.is_some()
    }

    fn metadata_bytes(&self) -> usize {
        self.call_id.as_deref().map_or(0, str::len)
            + self.name.as_deref().map_or(0, str::len)
            + self.kind.as_deref().map_or(0, str::len)
    }
}

struct ToolDelta<'a> {
    source: u64,
    id: Option<&'a str>,
    kind: Option<&'a str>,
    name: Option<&'a str>,
    arguments: Option<&'a str>,
}

impl<'a> ToolDelta<'a> {
    fn parse(call: &'a Value) -> StreamResult<Self> {
        let call = call.as_object().ok_or("Malformed upstream tool call")?;
        let source = call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("Invalid upstream tool index")?;
        let id = metadata(call.get("id"))?;
        let kind = metadata(call.get("type"))?;
        if kind.is_some_and(|kind| kind != "function") {
            return Err("Unsupported upstream tool type");
        }
        let function = call
            .get("function")
            .map(|v| v.as_object().ok_or("Malformed upstream function"))
            .transpose()?;
        let name = metadata(function.and_then(|f| f.get("name")))?;
        let arguments = function
            .and_then(|f| f.get("arguments"))
            .map(|v| v.as_str().ok_or("Malformed upstream tool arguments"))
            .transpose()?;
        Ok(Self {
            source,
            id,
            kind,
            name,
            arguments,
        })
    }
}

enum ItemKind {
    Reasoning,
    Text,
    Tool(Tool),
}

struct Item {
    id: String,
    kind: ItemKind,
    text: String,
    added: bool,
    // Once set, the item snapshot is immutable and is reused in final output.
    done_status: Option<&'static str>,
}

#[derive(Serialize)]
struct ContentPart<'a> {
    r#type: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    annotations: Option<[(); 0]>,
}

// Borrow the canonical strings, including when serializing final output.
// No second full output DOM is needed alongside the wire envelope.
struct ItemSnapshot<'a> {
    item: &'a Item,
    status: &'a str,
    added: bool,
}

impl Serialize for ItemSnapshot<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let item = self.item;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("id", &item.id)?;
        map.serialize_entry("status", self.status)?;
        match &item.kind {
            ItemKind::Tool(tool) => {
                map.serialize_entry("type", "function_call")?;
                map.serialize_entry("call_id", &tool.call_id)?;
                map.serialize_entry("name", &tool.name)?;
                map.serialize_entry("arguments", if self.added { "" } else { &item.text })?;
            }
            kind => {
                if matches!(kind, ItemKind::Reasoning) {
                    map.serialize_entry("type", "reasoning")?;
                    map.serialize_entry("summary", &[] as &[(); 0])?;
                } else {
                    map.serialize_entry("type", "message")?;
                    map.serialize_entry("role", "assistant")?;
                }
                let parts = [item.part(&item.text)];
                map.serialize_entry("content", if self.added { &parts[..0] } else { &parts[..] })?;
            }
        }
        map.end()
    }
}

impl Item {
    fn ready(&self) -> bool {
        match &self.kind {
            ItemKind::Tool(tool) => tool.ready(),
            _ => true,
        }
    }

    fn part<'a>(&self, text: &'a str) -> ContentPart<'a> {
        let kind = match self.kind {
            ItemKind::Reasoning => "reasoning_text",
            ItemKind::Text => "output_text",
            ItemKind::Tool(_) => unreachable!("tools have no content part"),
        };
        ContentPart {
            r#type: kind,
            text,
            annotations: matches!(self.kind, ItemKind::Text).then_some([]),
        }
    }

    fn snapshot(&self, status: &str, added: bool) -> Value {
        serde_json::to_value(ItemSnapshot {
            item: self,
            status,
            added,
        })
        .expect("item snapshot is JSON serializable")
    }

    fn text_event(&self, index: usize, done: bool, text: &str) -> Value {
        let family = match self.kind {
            ItemKind::Reasoning => "reasoning_text",
            ItemKind::Text => "output_text",
            ItemKind::Tool(_) => "function_call_arguments",
        };
        let mut event = json!({
            "type":format!("response.{family}.{}", if done { "done" } else { "delta" }),
            "item_id":self.id,"output_index":index
        });
        event[if !done {
            "delta"
        } else if family == "function_call_arguments" {
            "arguments"
        } else {
            "text"
        }] = json!(text);
        if !matches!(self.kind, ItemKind::Tool(_)) {
            event["content_index"] = json!(0);
        }
        if matches!(self.kind, ItemKind::Text) {
            event["logprobs"] = json!([]);
        }
        if done && let ItemKind::Tool(tool) = &self.kind {
            event["name"] = json!(tool.name);
        }
        event
    }
}

// Explicit queue accounting: inline enum storage plus the owned delta bytes.
// Tool metadata held outside the queue is charged separately until added.
enum PendingAction {
    Add(usize),
    Delta(usize, Box<str>),
    CloseSegment(usize),
}

impl PendingAction {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match self {
                Self::Delta(_, text) => text.len(),
                _ => 0,
            }
    }
}

struct ChatResponses {
    envelope: Value,
    items: Vec<Item>,
    tools: HashMap<u64, usize>,
    call_ids: HashMap<Box<str>, u64>,
    // Empty arguments are an observed fact, not an output reservation. Each
    // source shares the item cap and charges one u64 to the pending budget.
    empty_arguments: HashSet<u64>,
    reasoning: Option<usize>,
    message: Option<usize>,
    pending: VecDeque<PendingAction>,
    pending_bytes: usize,
    content_bytes: usize,
    max_content_bytes: usize,
    max_items: usize,
    ready: Vec<Bytes>,
    sequence: u64,
    finish: Option<&'static str>,
    usage: Option<Value>,
    /// : time-to-first-byte, captured at the first upstream chunk.
    ttfb_ms: Option<u64>,
}

#[derive(Clone, Copy)]
enum CloseStep {
    Text,
    Part,
    Item,
}

// This cursor owns no payload. Each pull builds exactly one event; the caller
// must yield it before pulling again. In particular, do not collect this cursor.
struct Terminal {
    status: &'static str,
    index: usize,
    step: CloseStep,
    finished: bool,
}

impl Terminal {
    fn next(&mut self, bridge: &mut ChatResponses) -> Option<Bytes> {
        if self.finished {
            return None;
        }
        while self.index < bridge.items.len() {
            let index = self.index;
            let item = &mut bridge.items[index];
            if !item.added || item.done_status.is_some() {
                self.index += 1;
                continue;
            }
            let event = match self.step {
                CloseStep::Text => {
                    self.step = if matches!(item.kind, ItemKind::Tool(_)) {
                        CloseStep::Item
                    } else {
                        CloseStep::Part
                    };
                    item.text_event(index, true, &item.text)
                }
                CloseStep::Part => {
                    self.step = CloseStep::Item;
                    json!({
                        "type":"response.content_part.done","item_id":item.id,"output_index":index,
                        "content_index":0,"part":item.part(&item.text)
                    })
                }
                CloseStep::Item => {
                    let status = if self.status == "completed" {
                        "completed"
                    } else {
                        "incomplete"
                    };
                    item.done_status = Some(status);
                    self.index += 1;
                    self.step = CloseStep::Text;
                    json!({
                        "type":"response.output_item.done","output_index":index,
                        "item":item.snapshot(status, false)
                    })
                }
            };
            return Some(bridge.encode(event));
        }
        self.finished = true;
        Some(bridge.final_event(self.status))
    }
}

#[derive(Serialize)]
struct FinalResponse<'a> {
    #[serde(flatten)]
    envelope: &'a Value,
    #[serde(serialize_with = "serialize_output")]
    output: &'a [Item],
}

fn serialize_output<S: Serializer>(items: &[Item], serializer: S) -> Result<S::Ok, S::Error> {
    let mut seq = serializer.serialize_seq(None)?;
    for item in items {
        if let Some(status) = item.done_status {
            seq.serialize_element(&ItemSnapshot {
                item,
                status,
                added: false,
            })?;
        }
    }
    seq.end()
}

#[derive(Serialize)]
struct FinalEvent<'a> {
    r#type: &'a str,
    sequence_number: u64,
    response: FinalResponse<'a>,
}

impl ChatResponses {
    fn new(
        model: String,
        request: Option<&Value>,
        max_content_bytes: usize,
        max_items: usize,
    ) -> Self {
        let mut envelope = json!({
            "id":format!("resp_{}", uuid::Uuid::new_v4().as_simple()),
            "object":"response","created_at":chrono::Utc::now().timestamp(),
            "status":"in_progress","model":model,"output":[],
            "error":null,"incomplete_details":null
        });
        convert::apply_request_context_defaults(envelope.as_object_mut().unwrap(), request);
        Self {
            envelope,
            items: Vec::new(),
            tools: HashMap::new(),
            call_ids: HashMap::new(),
            empty_arguments: HashSet::new(),
            reasoning: None,
            message: None,
            pending: VecDeque::new(),
            pending_bytes: 0,
            content_bytes: 0,
            max_content_bytes,
            max_items,
            ready: Vec::new(),
            sequence: 0,
            finish: None,
            usage: None,
            ttfb_ms: None,
        }
    }

    fn encode(&mut self, mut event: Value) -> Bytes {
        self.sequence += 1;
        event["sequence_number"] = json!(self.sequence);
        let kind = event["type"].as_str().unwrap().to_owned();
        responses_event(&kind, event)
    }

    fn emit(&mut self, event: Value) {
        let event = self.encode(event);
        self.ready.push(event);
    }

    fn check_item_limit(&self) -> StreamResult<()> {
        if self
            .items
            .len()
            .checked_add(self.empty_arguments.len())
            .is_none_or(|count| count >= self.max_items)
        {
            return Err("Upstream output item limit exceeded");
        }
        Ok(())
    }

    fn reserve(&mut self, kind: ItemKind) -> StreamResult<usize> {
        self.check_item_limit()?;
        let prefix = match kind {
            ItemKind::Reasoning => "rs",
            ItemKind::Text => "msg",
            ItemKind::Tool(_) => "fc",
        };
        let index = self.items.len();
        self.items.push(Item {
            id: format!("{prefix}_{}", uuid::Uuid::new_v4().as_simple()),
            kind,
            text: String::new(),
            added: false,
            done_status: None,
        });
        Ok(index)
    }

    fn queue(&mut self, action: PendingAction) -> StreamResult<()> {
        let blocked = matches!(action, PendingAction::Add(index) if !self.items[index].ready());
        if self.pending.is_empty() && !blocked {
            self.apply(action);
        } else {
            self.pending_bytes = checked_budget(
                self.pending_bytes,
                action.bytes(),
                PENDING_LIMIT,
                "Tool metadata buffer limit exceeded",
            )?;
            self.pending.push_back(action);
        }
        Ok(())
    }

    fn release(&mut self) {
        while let Some(action) = self.pending.front() {
            if matches!(action, PendingAction::Add(index) if !self.items[*index].ready()) {
                break;
            }
            let action = self.pending.pop_front().unwrap();
            self.pending_bytes -= action.bytes();
            self.apply(action);
        }
    }

    fn apply(&mut self, action: PendingAction) {
        match action {
            PendingAction::Add(index) => {
                let item = &mut self.items[index];
                item.added = true;
                if let ItemKind::Tool(tool) = &item.kind {
                    self.pending_bytes -= tool.metadata_bytes();
                }
                let snapshot = item.snapshot("in_progress", true);
                let part = if matches!(item.kind, ItemKind::Tool(_)) {
                    None
                } else {
                    Some(json!({
                        "type":"response.content_part.added","item_id":item.id,
                        "output_index":index,"content_index":0,"part":item.part("")
                    }))
                };
                self.emit(json!({"type":"response.output_item.added","output_index":index,"item":snapshot}));
                if let Some(part) = part {
                    self.emit(part);
                }
            }
            PendingAction::Delta(index, text) => {
                let item = &mut self.items[index];
                item.text.push_str(&text);
                let event = item.text_event(index, false, &text);
                self.emit(event);
            }
            PendingAction::CloseSegment(index) => self.close(index, "completed"),
        }
    }

    fn close(&mut self, index: usize, status: &'static str) {
        let item = &mut self.items[index];
        if !item.added || item.done_status.is_some() {
            return;
        }
        item.done_status = Some(status);
        let text_done = item.text_event(index, true, &item.text);
        let part_done = if matches!(item.kind, ItemKind::Tool(_)) {
            None
        } else {
            Some(json!({
                "type":"response.content_part.done","item_id":item.id,"output_index":index,
                "content_index":0,"part":item.part(&item.text)
            }))
        };
        let snapshot = item.snapshot(status, false);
        self.emit(text_done);
        if let Some(part) = part_done {
            self.emit(part);
        }
        self.emit(json!({"type":"response.output_item.done","output_index":index,"item":snapshot}));
    }

    fn close_reasoning(&mut self) -> StreamResult<()> {
        if let Some(index) = self.reasoning.take() {
            self.queue(PendingAction::CloseSegment(index))?;
        }
        Ok(())
    }

    fn close_message(&mut self) -> StreamResult<()> {
        if let Some(index) = self.message.take() {
            self.queue(PendingAction::CloseSegment(index))?;
        }
        Ok(())
    }

    fn delta(&mut self, index: usize, text: &str) -> StreamResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.content_bytes = checked_budget(
            self.content_bytes,
            text.len(),
            self.max_content_bytes,
            "Upstream output byte limit exceeded",
        )?;
        // Check before allocating the owned delta as well as before enqueuing.
        if !self.pending.is_empty() {
            checked_budget(
                self.pending_bytes,
                std::mem::size_of::<PendingAction>() + text.len(),
                PENDING_LIMIT,
                "Tool metadata buffer limit exceeded",
            )?;
        }
        self.queue(PendingAction::Delta(index, text.into()))
    }

    fn text(&mut self, text: &str, reasoning: bool) -> StreamResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.finish.is_some() {
            return Err("Content after upstream finish");
        }
        if reasoning {
            self.close_message()?;
        } else {
            self.close_reasoning()?;
        }
        let index = if let Some(index) = if reasoning {
            self.reasoning
        } else {
            self.message
        } {
            index
        } else {
            let index = self.reserve(if reasoning {
                ItemKind::Reasoning
            } else {
                ItemKind::Text
            })?;
            if reasoning {
                self.reasoning = Some(index);
            } else {
                self.message = Some(index);
            }
            self.queue(PendingAction::Add(index))?;
            index
        };
        self.delta(index, text)
    }

    fn tool_deltas<'a>(&self, calls: &'a Value) -> StreamResult<Vec<ToolDelta<'a>>> {
        let calls = calls.as_array().ok_or("Malformed upstream tool calls")?;
        let mut seen = HashSet::new();
        let mut call_ids = HashMap::new();
        calls
            .iter()
            .map(|call| {
                let delta = ToolDelta::parse(call)?;
                if !seen.insert(delta.source) {
                    return Err("Duplicate upstream tool index");
                }
                if let Some(id) = delta.id
                    && (self
                        .call_ids
                        .get(id)
                        .is_some_and(|source| *source != delta.source)
                        || call_ids
                            .insert(id, delta.source)
                            .is_some_and(|source| source != delta.source))
                {
                    return Err("Duplicate upstream tool call ID");
                }
                if let Some(index) = self.tools.get(&delta.source) {
                    let ItemKind::Tool(tool) = &self.items[*index].kind else {
                        unreachable!()
                    };
                    for (existing, incoming) in [
                        (tool.call_id.as_deref(), delta.id),
                        (tool.name.as_deref(), delta.name),
                        (tool.kind.as_deref(), delta.kind),
                    ] {
                        if let (Some(existing), Some(incoming)) = (existing, incoming)
                            && existing != incoming
                        {
                            return Err("Conflicting upstream tool metadata");
                        }
                    }
                }
                Ok(delta)
            })
            .collect()
    }

    fn tool(&mut self, delta: ToolDelta<'_>) -> StreamResult<()> {
        let ToolDelta {
            source,
            id,
            kind,
            name,
            arguments,
        } = delta;
        let nonempty = id.is_some()
            || kind.is_some()
            || name.is_some()
            || arguments.is_some_and(|s| !s.is_empty());
        if !nonempty && !self.tools.contains_key(&source) {
            if arguments.is_some() && !self.empty_arguments.contains(&source) {
                self.check_item_limit()?;
                self.pending_bytes = checked_budget(
                    self.pending_bytes,
                    std::mem::size_of::<u64>(),
                    PENDING_LIMIT,
                    "Tool metadata buffer limit exceeded",
                )?;
                self.empty_arguments.insert(source);
            }
            return Ok(());
        }
        if self.finish.is_some() {
            return Err("Tool data after upstream finish");
        }
        let (index, new) = match self.tools.get(&source) {
            Some(index) => (*index, false),
            None => {
                // Transfer, rather than duplicate, the source's bounded state.
                let saw_arguments = self.empty_arguments.remove(&source);
                if saw_arguments {
                    self.pending_bytes -= std::mem::size_of::<u64>();
                }
                let index = self.reserve(ItemKind::Tool(Tool {
                    saw_arguments,
                    ..Tool::default()
                }))?;
                self.tools.insert(source, index);
                (index, true)
            }
        };
        let item = &mut self.items[index];
        let ItemKind::Tool(tool) = &mut item.kind else {
            unreachable!()
        };
        for (slot, incoming) in [
            (&mut tool.call_id, id),
            (&mut tool.name, name),
            (&mut tool.kind, kind),
        ] {
            if let Some(incoming) = incoming
                && slot.is_none()
            {
                self.content_bytes = checked_budget(
                    self.content_bytes,
                    incoming.len(),
                    self.max_content_bytes,
                    "Upstream output byte limit exceeded",
                )?;
                if !item.added {
                    self.pending_bytes = checked_budget(
                        self.pending_bytes,
                        incoming.len(),
                        PENDING_LIMIT,
                        "Tool metadata buffer limit exceeded",
                    )?;
                }
                *slot = Some(incoming.into());
            }
        }
        tool.saw_arguments |= arguments.is_some();
        if let Some(id) = id
            && !self.call_ids.contains_key(id)
        {
            self.call_ids.insert(id.into(), source);
        }
        if nonempty {
            self.close_reasoning()?;
            self.close_message()?;
        }
        if new {
            self.queue(PendingAction::Add(index))?;
        }
        // A late metadata fragment may unblock ALL preceding actions, including
        // other tools and reasoning/text, before its own argument delta.
        self.release();
        if let Some(arguments) = arguments {
            self.delta(index, arguments)?;
        }
        Ok(())
    }

    fn frame(&mut self, data: &str) -> StreamResult<bool> {
        if data.trim() == "[DONE]" {
            return Ok(true);
        }
        let value: Value = serde_json::from_str(data).map_err(|_| "Invalid upstream SSE JSON")?;
        let object = value.as_object().ok_or("Malformed upstream Chat chunk")?;
        if object.get("error").is_some_and(|error| !error.is_null()) {
            return Err("Upstream service error");
        }
        let choices = object
            .get("choices")
            .map(|v| v.as_array().ok_or("Malformed upstream choices"))
            .transpose()?;
        if choices.is_none() && !object.contains_key("usage") {
            return Err("Missing upstream choices");
        }
        if let Some(choices) = choices {
            if choices.len() > 1 {
                return Err("Ambiguous upstream choices");
            }
            for choice in choices {
                let choice = choice.as_object().ok_or("Malformed upstream choice")?;
                if choice.get("index").is_some_and(|v| v.as_u64() != Some(0)) {
                    return Err("Invalid upstream choice index");
                }
                if let Some(delta) = choice.get("delta") {
                    let delta = delta.as_object().ok_or("Malformed upstream delta")?;
                    if self.finish.is_some() && !delta.is_empty() {
                        return Err("Nonempty delta after upstream finish");
                    }
                    // Validate the entire array before any segment transition:
                    // a later duplicate/conflict cannot complete active output.
                    let calls = delta
                        .get("tool_calls")
                        .map(|calls| self.tool_deltas(calls))
                        .transpose()?
                        .unwrap_or_default();
                    // Canonical ordering inside a chunk is R -> T -> tool array,
                    // not sorting by source tool index or guessing generation time.
                    for (field, reasoning) in [("reasoning_content", true), ("content", false)] {
                        if let Some(value) = delta.get(field).filter(|v| !v.is_null()) {
                            self.text(
                                value.as_str().ok_or("Malformed upstream text delta")?,
                                reasoning,
                            )?;
                        }
                    }
                    for call in calls {
                        self.tool(call)?;
                    }
                }
                if let Some(finish) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                    let finish = match finish.as_str() {
                        Some("stop") => "stop",
                        Some("tool_calls") => "tool_calls",
                        Some("length") => "length",
                        Some("content_filter") => "content_filter",
                        _ => return Err("Invalid upstream finish reason"),
                    };
                    if self.finish.is_some_and(|previous| previous != finish) {
                        return Err("Conflicting upstream finish reasons");
                    }
                    self.finish = Some(finish);
                }
            }
        }
        if let Some(usage) = object.get("usage").filter(|v| !v.is_null()) {
            self.usage = Some(usage.clone());
        }
        Ok(false)
    }

    fn terminal(&mut self, error: Option<&'static str>) -> Terminal {
        debug_assert!(
            self.ready.is_empty(),
            "drain validated events before terminal"
        );
        let truncated = matches!(self.finish, Some("length" | "content_filter"));
        let error = error.or_else(|| {
            if !self.pending.is_empty() || self.items.iter().any(|item| {
                matches!(&item.kind, ItemKind::Tool(tool) if !tool.ready() || !tool.saw_arguments)
            }) {
                Some("Incomplete upstream tool metadata")
            } else if self.finish.is_none() {
                Some("Missing upstream finish reason")
            } else if !truncated && self.items.iter().any(|item| {
                matches!(item.kind, ItemKind::Tool(_)) && !valid_arguments_json(&item.text)
            }) {
                Some("Invalid upstream tool arguments JSON")
            } else { None }
        });
        let status = if error.is_some() {
            "failed"
        } else if truncated {
            "incomplete"
        } else {
            "completed"
        };
        // Do not invent legal items for unresolved reservations or emit queued
        // deltas after failure. Final output reflects only what was published.
        self.pending.clear();
        self.envelope["status"] = json!(status);
        if let Some(error) = error {
            self.envelope["error"] = json!({"code":"server_error","message":error});
            tracing::warn!(
                target:"llm_proxy::proxy", correlation_id = %self.envelope["id"].as_str().unwrap(),
                direction = "chat-to-responses", diagnostic = error, "Chat SSE conversion failed"
            );
        } else if truncated {
            self.envelope["incomplete_details"] = json!({"reason":
                if self.finish == Some("length") { "max_output_tokens" } else { "content_filter" }
            });
        }
        match convert::map_responses_usage(self.usage.as_ref()) {
            Ok(usage) => self.envelope["usage"] = usage,
            Err(issues) => tracing::warn!(
                target:"llm_proxy::convert", correlation_id = %self.envelope["id"].as_str().unwrap(),
                missing_fields = ?issues.missing_fields, invalid_fields = ?issues.invalid_fields,
                "upstream usage incomplete or invalid, omitted client usage without fabricating counts"
            ),
        }
        Terminal {
            status,
            index: 0,
            step: CloseStep::Text,
            finished: false,
        }
    }

    fn final_event(&mut self, status: &str) -> Bytes {
        self.sequence += 1;
        // Replace the empty output field with a borrowed serializer, not a
        // cloned output array followed by another clone into the event.
        self.envelope.as_object_mut().unwrap().remove("output");
        let kind = format!("response.{status}");
        let event = FinalEvent {
            r#type: &kind,
            sequence_number: self.sequence,
            response: FinalResponse {
                envelope: &self.envelope,
                output: &self.items,
            },
        };
        let mut wire = format!("event: {kind}\ndata: ").into_bytes();
        serde_json::to_writer(&mut wire, &event).expect("final response is JSON serializable");
        wire.extend_from_slice(b"\n\n");
        Bytes::from(wire)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_usage(
        &self,
        state: &AppState,
        model: String,
        provider: String,
        endpoint: String,
        token: Option<&crate::token::auth::TokenContext>,
        routed_target: Option<String>,
        latency_ms: Option<i64>,
    ) {
        let usage = self.usage.as_ref();
        let input = usage
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .filter(|v| *v >= 0);
        let output = usage
            .and_then(|u| u.get("completion_tokens"))
            .and_then(Value::as_i64)
            .filter(|v| *v >= 0);
        if let (Some(input), Some(output)) = (input, output)
            && input.checked_add(output).is_some()
        {
            let (cache_hit, cache_miss) = usage
                .map(crate::usage_stats::extract_cache_tokens)
                .unwrap_or((None, None));
            // : detect truncation from Chat finish_reason.
            let (truncated, truncation_reason) = match self.finish {
                Some("length") => (true, Some("max_tokens")),
                Some("content_filter") => (true, Some("content_filter")),
                _ => (false, None),
            };
            state.record_usage_with_status_and_target(
                model,
                provider,
                endpoint,
                input,
                output,
                cache_hit,
                cache_miss,
                latency_ms,
                token,
                Some("completed"),
                true,
                0,
                truncated,
                truncation_reason,
                self.ttfb_ms,
                routed_target,
            );
            return;
        }
        let mut fields = Vec::new();
        if usage.is_none() {
            fields.push("usage");
        } else {
            if input.is_none() {
                fields.push("prompt_tokens");
            }
            if output.is_none() {
                fields.push("completion_tokens");
            }
            if input.is_some() && output.is_some() {
                fields.push("total_tokens_overflow");
            }
        }
        tracing::warn!(
            target:"llm_proxy::proxy", correlation_id = %self.envelope["id"].as_str().unwrap(),
            missing_or_invalid_fields = ?fields,
            "upstream usage incomplete or invalid, omitted fabricated counts from internal storage"
        );
    }
}

// Deserialize without a DOM, but retain the old serde_json depth, number and
// Unicode checks. IgnoredAny bypasses those checks in serde_json's fast skip
// path. This visitor retains only parser stack/scratch (escaped strings), not
// a Value/Vec/Map per JSON node.
struct JsonSyntax;

impl<'de> Deserialize<'de> for JsonSyntax {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(Self)
    }
}

impl<'de> Visitor<'de> for JsonSyntax {
    type Value = Self;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<Self>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        while map.next_key::<Self>()?.is_some() {
            map.next_value::<Self>()?;
        }
        Ok(self)
    }
}

fn valid_arguments_json(text: &str) -> bool {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    JsonSyntax::deserialize(&mut deserializer)
        .and_then(|_| deserializer.end())
        .is_ok()
}

/// Lenient on upstream responses: absent, null, and empty-string metadata all
/// mean "not provided in this chunk" — Bailian sends explicit `""` where the
/// OpenAI spec omits the field. Non-string non-null values remain malformed.
fn metadata(value: Option<&Value>) -> StreamResult<Option<&str>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err("Invalid upstream tool metadata"),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn chat_sse_to_responses_sse(
    state: &AppState,
    upstream: reqwest::Response,
    frontend_model: String,
    provider_id: String,
    endpoint: String,
    original_responses_req: Option<&Value>,
    token: Option<&crate::token::auth::TokenContext>,
    request_start: std::time::Instant,
    routed_target: Option<String>,
) -> Response {
    let token_owned = token.cloned();
    let cfg = state.config();
    let mut bridge = ChatResponses::new(
        frontend_model.clone(),
        original_responses_req,
        cfg.server_config()
            .map(|s| s.max_sse_buffer_bytes)
            .unwrap_or(64 * 1024 * 1024),
        cfg.server_config()
            .map(|s| s.max_output_items)
            .unwrap_or(4096),
    );
    let state = state.clone();
    let mut upstream = upstream.bytes_stream();
    let body_stream = stream! {
        bridge.emit(json!({"type":"response.created","response":bridge.envelope}));
        bridge.emit(json!({"type":"response.in_progress","response":bridge.envelope}));
        for event in std::mem::take(&mut bridge.ready) {
            yield Ok::<Bytes, Infallible>(event);
        }
        let mut frames = DataFrames::default();
        let mut failure = None;
        let mut saw_done = false;
        let mut ttfb_recorded = false;
        'read: while let Some(chunk) = upstream.next().await {
            let chunk = match chunk {
                Ok(chunk) => {
                    if !ttfb_recorded {
                        bridge.ttfb_ms = Some(request_start.elapsed().as_millis() as u64);
                        ttfb_recorded = true;
                    }
                    chunk
                }
                Err(_) => {
                    // A complete standalone-CR frame precedes the transport
                    // error. In particular, [DONE] still ignores what follows.
                    if let Some(data) = frames.finish_cr() {
                        let result = bridge.frame(&data);
                        for event in std::mem::take(&mut bridge.ready) {
                            yield Ok(event);
                        }
                        match result {
                            Ok(true) => { saw_done = true; break; }
                            Err(error) => { failure = Some(error); break; }
                            Ok(false) => {}
                        }
                    }
                    state.observe_stream_interruption("chat-to-responses", "Upstream stream transport error");
                    failure = Some("Upstream stream transport error");
                    break;
                }
            };
            for byte in chunk {
                let result = frames.byte(byte, bridge.max_content_bytes)
                    .and_then(|frame| frame.map_or(Ok(false), |data| bridge.frame(&data)));
                // Always deliver already-validated events before the one terminal.
                for event in std::mem::take(&mut bridge.ready) {
                    yield Ok(event);
                }
                match result {
                    Ok(true) => { saw_done = true; break 'read; }
                    Ok(false) => {}
                    Err(error) => { failure = Some(error); break 'read; }
                }
            }
        }
        if failure.is_none() && !saw_done {
            failure = frames.eof()
                .and_then(|frame| frame.map_or(Ok(false), |data| bridge.frame(&data)))
                .err();
            for event in std::mem::take(&mut bridge.ready) {
                yield Ok(event);
            }
        }
        let mut terminal = bridge.terminal(failure);
        bridge.record_usage(
            &state,
            frontend_model,
            provider_id,
            endpoint,
            token_owned.as_ref(),
            routed_target,
            Some(request_start.elapsed().as_millis() as i64),
        );
        while let Some(event) = terminal.next(&mut bridge) {
            yield Ok(event);
        }
    };
    sse_response(Body::from_stream(body_stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_wire_budget_exact_lf_crlf_boundaries() {
        for ending in ["\n", "\r\n"] {
            for body in ["data: x", ": comment\nevent: ignored\ndata: x\ndata: y"] {
                let frame = format!("{}{ending}{ending}", body.replace('\n', ending));
                for limit in [frame.len() - 1, frame.len(), frame.len() + 1] {
                    let mut frames = DataFrames::default();
                    let mut dispatched = Vec::new();
                    let result = frame.bytes().try_for_each(|byte| {
                        if let Some(data) = frames.byte(byte, limit)? {
                            dispatched.push(data);
                        }
                        Ok::<_, &'static str>(())
                    });
                    if limit < frame.len() {
                        assert_eq!(result, Err("SSE frame limit exceeded"), "{frame:?}");
                        assert!(dispatched.is_empty(), "over-budget frame must not dispatch");
                    } else {
                        assert_eq!(result, Ok(()));
                        assert_eq!(dispatched.len(), 1);
                        assert_eq!(frames.frame_bytes, 0, "reset after the entire separator");
                        // A second exactly-sized frame must not inherit the LF
                        // from the preceding blank-line CRLF.
                        for byte in frame.bytes() {
                            if let Some(data) = frames.byte(byte, frame.len()).unwrap() {
                                dispatched.push(data);
                            }
                        }
                        assert_eq!(dispatched.len(), 2);
                        frames.eof().unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn frame_wire_budget_many_short_lines() {
        for ending in ["\n", "\r\n"] {
            for line in [":", "x", "data:"] {
                let prefix = format!("{line}{ending}").repeat(4096);
                let wire = format!("{prefix}{ending}");
                let mut frames = DataFrames::default();
                let limit = prefix.len() - 1;
                for (offset, byte) in wire.bytes().enumerate() {
                    let result = frames.byte(byte, limit);
                    if offset == limit {
                        assert_eq!(result, Err("SSE frame limit exceeded"));
                        break;
                    }
                    assert_eq!(result, Ok(None), "nonblank lines do not reset budget");
                }
            }
        }
        // Conversely, arbitrarily many *frames* do not share one budget.
        let mut frames = DataFrames::default();
        for byte in ":\r\n\r\n".repeat(4096).bytes() {
            assert_eq!(frames.byte(byte, 5), Ok(None));
        }
        assert_eq!(frames.frame_bytes, 0);
    }

    #[test]
    fn large_arguments_terminal_is_pull_driven() {
        // Many small JSON values would amplify a DOM allocation; escaping in
        // the second payload also makes wire bytes exceed the content budget.
        let arguments = [
            format!("[{}0]", "0,".repeat(256 * 1024)),
            serde_json::to_string(&"\n\\\"".repeat(256 * 1024)).unwrap(),
        ];
        let content_limit = arguments
            .iter()
            .enumerate()
            .map(|(index, arguments)| {
                arguments.len() + format!("call_{index}").len() + "function".len() + "f".len()
            })
            .sum();
        let mut bridge = ChatResponses::new("fixture".into(), None, content_limit, 8);
        for (index, arguments) in arguments.iter().enumerate() {
            for fragment in arguments.as_bytes().chunks(64 * 1024) {
                let frame = json!({"choices":[{"delta":{"tool_calls":[{
                    "index":index,"id":format!("call_{index}"),"type":"function",
                    "function":{"name":"f","arguments":std::str::from_utf8(fragment).unwrap()}
                }]}}]})
                .to_string();
                assert!(frame.len() + "data: \n\n".len() <= content_limit);
                bridge.frame(&frame).unwrap();
                bridge.ready.clear();
            }
        }
        assert_eq!(
            bridge.content_bytes, content_limit,
            "exact cumulative content budget"
        );
        bridge.finish = Some("tool_calls");
        let mut terminal = bridge.terminal(None);
        assert!(
            bridge.ready.is_empty(),
            "terminal preparation must not queue arguments done, item done and final payloads"
        );
        assert_eq!(bridge.envelope["output"], json!([]));
        assert!(bridge.items.iter().all(|item| item.done_status.is_none()));
        let pointers: Vec<_> = bridge.items.iter().map(|item| item.text.as_ptr()).collect();
        let mut sequence = bridge.sequence;
        for (index, arguments) in arguments.iter().enumerate() {
            for kind in [
                "response.function_call_arguments.done",
                "response.output_item.done",
            ] {
                let bytes = terminal.next(&mut bridge).unwrap();
                sequence += 1;
                assert_eq!(
                    bridge.sequence, sequence,
                    "exactly one event built per pull"
                );
                assert!(bridge.ready.is_empty(), "no serialized payload backlog");
                assert_eq!(
                    bridge.envelope["output"],
                    json!([]),
                    "no early final output DOM"
                );
                assert!(
                    bridge.items[index + 1..]
                        .iter()
                        .all(|item| item.done_status.is_none()),
                    "later items are not closed/serialized ahead of the consumer"
                );
                let event = super::super::tests::sse_events(&bytes).pop().unwrap();
                assert_eq!(event["type"], kind);
                assert_eq!(event["sequence_number"], sequence);
                assert_eq!(event["output_index"], index);
                let actual = if kind == "response.function_call_arguments.done" {
                    assert!(bridge.items[index].done_status.is_none());
                    &event["arguments"]
                } else {
                    assert_eq!(bridge.items[index].done_status, Some("completed"));
                    assert_eq!(
                        event["item"],
                        bridge.items[index].snapshot("completed", false)
                    );
                    &event["item"]["arguments"]
                };
                assert_eq!(actual.as_str().unwrap(), arguments);
                // Dropping the event before next() models downstream backpressure.
            }
        }
        let final_bytes = terminal.next(&mut bridge).unwrap();
        assert!(
            final_bytes.len() > content_limit,
            "serialization overhead is not content"
        );
        let event = super::super::tests::sse_events(&final_bytes).pop().unwrap();
        assert_eq!(event["type"], "response.completed");
        assert_eq!(event["sequence_number"], sequence + 1);
        for (index, item) in bridge.items.iter().enumerate() {
            assert_eq!(
                event["response"]["output"][index],
                item.snapshot("completed", false)
            );
            assert_eq!(
                item.text.as_ptr(),
                pointers[index],
                "borrow canonical storage"
            );
        }
        assert!(
            bridge.envelope.get("output").is_none(),
            "final serialization borrows output"
        );
        assert!(bridge.ready.is_empty());
        assert!(terminal.next(&mut bridge).is_none(), "one terminal only");
    }

    #[test]
    fn non_dom_arguments_preserve_parser_checks() {
        for text in [
            "null",
            "true",
            "false",
            "-1",
            "18446744073709551615",
            "1.25e3",
            "\"text\"",
            "\"\\uD83D\\uDE00\"",
            "{\"nested\":[{},[]]}",
            "",
            "[1,]",
            "{\"x\":}",
            "00",
            "1e999",
            "\"\\uD800\"",
            "\"\\uDC00\"",
            "\"\\uZZZZ\"",
            "\"unescaped\n\"",
            "{} {}",
            "true trailing",
            "0\0",
        ] {
            assert_eq!(
                valid_arguments_json(text),
                serde_json::from_str::<Value>(text).is_ok(),
                "{text:?}"
            );
        }
        for depth in [126, 127, 128, 129, 1024] {
            let text = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(
                valid_arguments_json(&text),
                serde_json::from_str::<Value>(&text).is_ok(),
                "preserve the existing serde_json nesting limit at depth {depth}"
            );
        }
    }

    #[test]
    fn empty_argument_observations_share_item_cap_without_reserving() {
        let mut bridge = ChatResponses::new("fixture".into(), None, PENDING_LIMIT, 3);
        for source in [u64::MAX, 7, 99] {
            let frame = json!({"choices":[{"delta":{"tool_calls":[
                {"index":source,"function":{"arguments":""}}
            ]}}]});
            for _ in 0..16 {
                assert_eq!(bridge.frame(&frame.to_string()), Ok(false));
            }
        }
        assert!(
            bridge.items.is_empty(),
            "empty observations reserve no output"
        );
        assert!(
            bridge.pending.is_empty(),
            "empty observations block no events"
        );
        assert!(
            bridge.ready.is_empty(),
            "empty observations publish no events"
        );
        assert_eq!(
            bridge.frame(
                &json!({"choices":[{"delta":{"tool_calls":[
                    {"index":8,"function":{"arguments":""}}
                ]}}]})
                .to_string()
            ),
            Err("Upstream output item limit exceeded")
        );
        assert_eq!(bridge.pending_bytes, 3 * std::mem::size_of::<u64>());
    }

    #[test]
    fn empty_argument_observations_share_pending_cap() {
        let action = std::mem::size_of::<PendingAction>();
        let observation = std::mem::size_of::<u64>();
        let mut bridge = ChatResponses::new("fixture".into(), None, 4 * PENDING_LIMIT, 32);
        let arguments = "x".repeat(PENDING_LIMIT - 2 * action - observation);
        bridge
            .frame(
                &json!({"choices":[{"delta":{"tool_calls":[
                    {"index":1,"function":{"arguments":arguments}}
                ]}}]})
                .to_string(),
            )
            .unwrap();
        let empty = json!({"choices":[{"delta":{"tool_calls":[
            {"index":u64::MAX,"function":{"arguments":""}}
        ]}}]});
        for _ in 0..16 {
            assert_eq!(bridge.frame(&empty.to_string()), Ok(false));
        }
        assert_eq!(
            bridge.frame(
                &json!({"choices":[{"delta":{"tool_calls":[
                    {"index":8,"function":{"arguments":""}}
                ]}}]})
                .to_string()
            ),
            Err("Tool metadata buffer limit exceeded")
        );
        assert_eq!(bridge.pending_bytes, PENDING_LIMIT);
        assert_eq!(bridge.items.len(), 1);
        assert_eq!(bridge.pending.len(), 2);
        assert!(bridge.ready.is_empty());
    }

    #[test]
    fn pending_budget_exact_boundary_and_metadata_release() {
        let action = std::mem::size_of::<PendingAction>();
        for extra in [0, 1] {
            let mut bridge = ChatResponses::new("fixture".into(), None, 4 * PENDING_LIMIT, 32);
            let arguments = "x".repeat(PENDING_LIMIT - 2 * action + extra);
            let frame = json!({"choices":[{"delta":{"tool_calls":[
                {"index":8,"function":{"arguments":arguments}}
            ]}}]});
            let result = bridge.frame(&frame.to_string());
            if extra == 0 {
                assert_eq!(result, Ok(false));
                assert_eq!(bridge.pending_bytes, PENDING_LIMIT);
                assert!(bridge.ready.is_empty(), "no guessed metadata");
            } else {
                assert_eq!(result, Err("Tool metadata buffer limit exceeded"));
            }
        }
        let mut bridge = ChatResponses::new("fixture".into(), None, 4 * PENDING_LIMIT, 32);
        // Leave exactly enough room for late id/name/type. Added and delta
        // release their charges, so later large deltas do not accumulate here.
        let metadata_bytes = "call".len() + "f".len() + "function".len();
        let arguments = "x".repeat(PENDING_LIMIT - 2 * action - metadata_bytes);
        bridge
            .frame(
                &json!({"choices":[{"delta":{"tool_calls":[
                    {"index":8,"function":{"arguments":arguments}}
                ]}}]})
                .to_string(),
            )
            .unwrap();
        bridge
            .frame(
                &json!({"choices":[{"delta":{"tool_calls":[
                    {"index":8,"id":"call","type":"function","function":{"name":"f"}}
                ]}}]})
                .to_string(),
            )
            .unwrap();
        assert_eq!(bridge.pending_bytes, 0);
        assert!(bridge.pending.is_empty());
        assert_eq!(bridge.items[0].text, arguments);
        assert_eq!(
            bridge.ready.len(),
            2,
            "added plus exactly one arguments delta"
        );
        assert_eq!(
            checked_budget(usize::MAX, 1, usize::MAX, "overflow"),
            Err("overflow")
        );
    }

    #[test]
    fn bailian_empty_string_tool_metadata_is_absent() {
        // Real Bailian Coding Plan chunk shapes (captured 2026-09-09): the
        // first chunk carries id+name; continuation chunks send explicit
        // empty-string `id` and omit `name`; a trailing chunk repeats empty
        // arguments. Lenient upstream parsing treats null/"" as absent.
        let mut bridge = ChatResponses::new("fixture".into(), None, PENDING_LIMIT, 8);
        let frames = [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_0272","type":"function","function":{"name":"shell","arguments":""}}],"content":"","reasoning_content":""},"index":0,"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"","type":"function","function":{"arguments":"{\"command\": "}}],"content":"","reasoning_content":""},"index":0,"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"","type":"function","function":{"arguments":"\n[\"echo TOOL_PROBE_OK_7f3a\"]\n\n"}}],"content":"","reasoning_content":""},"index":0,"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"","type":"function","function":{"arguments":"}"}}],"content":"","reasoning_content":""},"index":0,"finish_reason":null}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"function":{"arguments":""},"index":0,"id":"","type":"function"}],"content":""},"index":0,"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"index":0,"finish_reason":"tool_calls"}]}),
        ];
        for frame in &frames {
            bridge.frame(&frame.to_string()).unwrap();
            bridge.ready.clear();
        }
        assert_eq!(bridge.finish, Some("tool_calls"));
        assert_eq!(bridge.items.len(), 1);
        let expected = "{\"command\": \n[\"echo TOOL_PROBE_OK_7f3a\"]\n\n}";
        assert_eq!(bridge.items[0].text, expected);

        let mut terminal = bridge.terminal(None);
        let done = terminal.next(&mut bridge).unwrap();
        let event = super::super::tests::sse_events(&done).pop().unwrap();
        assert_eq!(event["type"], "response.function_call_arguments.done");
        assert_eq!(event["arguments"], expected);
        let item_done = terminal.next(&mut bridge).unwrap();
        let event = super::super::tests::sse_events(&item_done).pop().unwrap();
        assert_eq!(event["type"], "response.output_item.done");
        assert_eq!(event["item"]["status"], "completed");
        assert_eq!(event["item"]["arguments"], expected);
        assert_eq!(event["item"]["call_id"], "call_0272");
        let completed = terminal.next(&mut bridge).unwrap();
        let event = super::super::tests::sse_events(&completed).pop().unwrap();
        assert_eq!(event["type"], "response.completed");
        assert_eq!(event["response"]["output"][0]["arguments"], expected);
    }

    #[test]
    fn non_string_tool_metadata_remains_malformed() {
        // Lenient parsing stops at structurally wrong metadata: numeric or
        // object id/name values are still rejected, not guessed as absent.
        for id in [json!(42), json!({}), json!([])] {
            let mut bridge = ChatResponses::new("fixture".into(), None, PENDING_LIMIT, 8);
            let frame = json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":id,"type":"function","function":{"name":"f","arguments":""}}
            ]}}]});
            assert_eq!(
                bridge.frame(&frame.to_string()),
                Err("Invalid upstream tool metadata")
            );
        }
    }
}
