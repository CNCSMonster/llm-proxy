# ADR-016: Token 刷新区分永久失败/瞬态失败，并发刷新聚合

## Status

Accepted

## Date

2026-07-04

## Context

`TokenManager.AccessToken()` 在 access token 过期或即将过期时调用 `refreshAccessToken()` 通过 OAuth refresh token 获取新 token。当前实现有两个问题：

1. **刷新失败无重试**：`refreshAccessToken()` 遇到任何错误（包括网络瞬态故障）直接返回失败，导致上层用户请求跟着失败，即使 1 秒后重试就可能成功。
2. **并发刷新无聚合**：多个并发请求同时发现 token 过期时，各 goroutine 通过 `sync.Mutex` 串行化刷新——第一个拿到锁的刷新之后，后续者看到缓存有效直接返回。但如果第一个刷新因网络瞬态失败，第二个出列的会发起一次新的 HTTP 调用，造成重复请求。

参考 CLIProxyAPI（`internal/auth/codex/openai_auth.go`）的 `RefreshTokensWithRetry` + `singleflight.Group`：
- `RefreshTokensWithRetry`：最多重试 N 次，线性退避（1s, 2s, 3s...），检测 `refresh_token_reused` 为不可重试的永久失败
- `singleflight.Group.Do(key, fn)`：相同 key 的并发调用只执行一次 fn，所有调用者共享结果

## Decision

为 `AccessToken()` 的刷新路径增加两级保护：

**第一级：singleflight 聚合**

```go
var tokenRefreshGroup singleflight.Group

func (tm *TokenManager) AccessToken(ctx context.Context, name string) (string, error) {
    // ... cache check ...
    result, err, _ := tokenRefreshGroup.Do(name, func() (interface{}, error) {
        return tm.refreshTokenWithRetry(ctx, name)
    })
    return result.(string), err
}
```

同一 provider `name` 的并发刷新请求共享一次 HTTP 调用，消除重复请求。

**第二级：带分类的重试**

```go
func (tm *TokenManager) refreshTokenWithRetry(ctx context.Context, name string) (string, error) {
    const maxRetries = 3
    for attempt := 0; attempt <= maxRetries; attempt++ {
        if attempt > 0 {
            // 线性退避 1s, 2s, 3s
            time.Sleep(time.Duration(attempt) * time.Second)
        }
        token, expiresIn, newRT, newID, err := refreshAccessToken(ctx, acct.RefreshToken)
        if err == nil { /* success */ }
        if isPermanentRefreshError(err) { return "", err }  // 永久失败 → 立即返回
    }
    return "", fmt.Errorf("token refresh failed after %d attempts", maxRetries+1)
}

func isPermanentRefreshError(err error) bool {
    // refresh_token 相关的不可恢复错误
    return strings.Contains(err.Error(), "refresh_token_reused") ||
           strings.Contains(err.Error(), "refresh_token_revoked") ||
           strings.Contains(err.Error(), "refresh_token_expired")
}
```

## Alternatives Considered

### 仅用 mutex 不加 singleflight

当前做法。优点是实现简单。缺点是在并发场景下，若第一次刷新失败，后续 goroutine 会各自发起新的 HTTP 调用，造成不必要的服务端压力。

### 仅用 singleflight 不加重试

比当前好但仍有缺陷——如果 singleflight 聚合后的那次调用遇到网络瞬态故障，所有等待者一起失败。

### singleflight + 无限重试

可以最大程度抗网络抖动，但没有终止条件。永久失败（如 refresh token 已被撤销）会一直重试到超时，用户体验差且浪费资源。

## Consequences

- 同一 provider 的并发 token 刷新只发一次 HTTP 请求（`singleflight` 聚合）
- 网络瞬态故障（DNS/超时/连接重置）自动重试 1-3 次，对上层调用者透明
- `refresh_token_reused` / `refresh_token_revoked` / `refresh_token_expired` 识别为永久失败，直接报错提示用户重新登录
- 需注意：`singleflight.Do` 使用的 ctx 应使用 `context.WithoutCancel(ctx)` 剥离上游请求的 cancel，防止上游请求超时导致刷新中断（但 token 仍需刷新供后续请求使用）——参照 CLIProxyAPI 的做法

## 单次实现细节：singleflight 原理

`golang.org/x/sync/singleflight` 的核心是一个 `Group`：

```go
type Group struct {
    mu sync.Mutex
    m  map[string]*call  // key → 正在进行中的调用
}

type call struct {
    wg  sync.WaitGroup
    val interface{}
    err error
}

func (g *Group) Do(key string, fn func() (interface{}, error)) (interface{}, error, bool) {
    g.mu.Lock()
    if c, ok := g.m[key]; ok {
        // key 已存在 → 有人正在执行
        g.mu.Unlock()
        c.wg.Wait()          // 等待它完成
        return c.val, c.err, true  // 共享结果，shared=true
    }
    c := new(call)
    c.wg.Add(1)
    g.m[key] = c            // 注册 key
    g.mu.Unlock()

    c.val, c.err = fn()     // 执行实际工作
    c.wg.Done()             // 唤醒所有等待者

    g.mu.Lock()
    delete(g.m, key)        // 清理，允许后续调用重新执行
    g.mu.Unlock()

    return c.val, c.err, false  // shared=false
}
```

**执行时序**（3 个 goroutine 同时刷新同一个 token）：

```
G1: Do("openai-sub") → 创建 call, 注册 key, 执行 refreshAccessToken()
G2: Do("openai-sub") → 发现 key 已存在 → c.wg.Wait() → 阻塞
G3: Do("openai-sub") → 发现 key 已存在 → c.wg.Wait() → 阻塞

        ... G1 的 HTTP 调用完成 ...

G1: c.wg.Done() → 唤醒 G2, G3 → delete key
G2: 拿到 c.val, c.err（与 G1 完全相同）
G3: 拿到 c.val, c.err（与 G1 完全相同）

结果：3 个 goroutine，1 次 HTTP 调用
```
