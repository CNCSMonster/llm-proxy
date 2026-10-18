//! State and navigation for the built-in document browser.

pub struct DocTuiState {
    sections: &'static [(&'static str, &'static str)],
    pub selected: usize,
    pub scroll: u16,
    viewport_width: u16,
    viewport_height: u16,
}

impl DocTuiState {
    pub fn new(sections: &'static [(&'static str, &'static str)]) -> Self {
        Self {
            sections,
            selected: 0,
            scroll: 0,
            viewport_width: 80,
            viewport_height: 24,
        }
    }

    /// 更新视口尺寸并按新视口重 clamp 滚动位置（启动与 resize 时调用）。
    pub fn set_viewport(&mut self, width: u16, height: u16) {
        self.viewport_width = width;
        self.viewport_height = height;
        self.clamp_scroll();
    }

    #[cfg(test)]
    pub fn from_sections(sections: &'static [(&'static str, &'static str)]) -> Self {
        Self::new(sections)
    }

    pub fn section_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.sections.iter().map(|(name, _)| *name)
    }

    pub fn selected_name(&self) -> &'static str {
        self.sections[self.selected].0
    }

    pub fn selected_text(&self) -> &'static str {
        self.sections[self.selected].1
    }

    pub fn next_section(&mut self) {
        self.selected = (self.selected + 1) % self.sections.len();
        self.scroll = 0;
    }

    pub fn previous_section(&mut self) {
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.sections.len() - 1);
        self.scroll = 0;
    }

    pub fn scroll_down(&mut self, amount: u16) {
        let max_scroll = self.max_scroll();
        self.scroll = self.scroll.saturating_add(amount).min(max_scroll);
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    /// 滚动上限 = 折行后的显示总行数 - 文档区可视行数。
    ///
    /// 渲染层开启 wrap（`Wrap { trim: false }`），源行数 ≠ 显示行数；
    /// 若按源行数 clamp，窄终端下折行变多，文末若干屏会永远滚不到。
    fn max_scroll(&self) -> u16 {
        use ratatui::widgets::{Paragraph, Wrap};
        // 文档区宽度 = 终端宽 - sections 侧栏(24) - 文档区内边框(2)
        let doc_width = self.viewport_width.saturating_sub(26).max(1);
        let line_count = Paragraph::new(self.selected_text())
            .wrap(Wrap { trim: false })
            .line_count(doc_width);
        // 文档区高度 = 终端高 - 帮助栏(1) - 文档区内边框(2)
        let usable = (self.viewport_height as usize).saturating_sub(3);
        line_count.saturating_sub(usable).min(u16::MAX as usize) as u16
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }
}

#[cfg(test)]
mod tests {
    use crate::doc;

    use super::*;

    #[test]
    fn includes_every_embedded_section_in_display_order() {
        let state = DocTuiState::new(doc::sections());
        assert_eq!(
            state.section_names().collect::<Vec<_>>(),
            [
                "cli-guide",
                "tui-guide",
                "troubleshooting",
                "oauth-accounts"
            ]
        );
    }

    #[test]
    fn moving_sections_resets_document_scroll() {
        let mut state = DocTuiState::new(doc::sections());
        state.scroll_down(20);
        state.next_section();

        assert_eq!(state.selected, 1);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn scroll_is_clamped_to_the_document_end() {
        let mut state = DocTuiState::from_sections(&[("short", "one\ntwo")]);
        // 小视口：可视 1 行，2 行文档最多滚到 1
        state.set_viewport(80, 4);
        state.scroll_down(100);

        assert_eq!(state.scroll, 1);
    }

    #[test]
    fn scroll_limit_accounts_for_wrapped_lines() {
        let mut state = DocTuiState::from_sections(&[("wrap", "aaaa bbbb cccc dddd")]);
        // 文档区宽 14：源 1 行折成 2 行，可视 1 行 → 最多滚 1；
        // 若按源行数 clamp 则上限为 0，本断言即验证 wrap 口径
        state.set_viewport(40, 4);
        state.scroll_down(100);

        assert_eq!(state.scroll, 1);
    }

    #[test]
    fn resize_reclamps_scroll_to_the_new_viewport() {
        let mut state = DocTuiState::from_sections(&[("wrap", "aaaa bbbb cccc dddd")]);
        state.set_viewport(40, 4);
        state.scroll_down(100);
        assert_eq!(state.scroll, 1);

        // 加宽后折行消失，上限归零，既有滚动值被重 clamp
        state.set_viewport(120, 4);
        assert_eq!(state.scroll, 0);
    }
}
