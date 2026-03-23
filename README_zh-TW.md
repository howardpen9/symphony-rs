# symphony-rs

[English](README.md) | **繁體中文**

以 Rust 重新實作 [OpenAI Symphony](https://github.com/openai/symphony)，依照公開的 `SPEC.md` 規格與 Elixir 參考行為開發。

## 這個專案在做什麼？

Symphony 是一個**長時間運行的自動化服務**，用來串接 issue tracker（目前支援 Linear）與 AI coding agent（Codex），自動完成軟體開發任務。

整體流程：

1. **輪詢 Issue** — 定期從 Linear 拉取待處理的 issue
2. **分派工作** — 根據設定（並行上限、重試策略）決定哪些 issue 可以開始處理
3. **建立工作區** — 為每個 issue 建立獨立的 workspace，確保路徑安全與隔離
4. **啟動 Agent** — 透過 Codex app-server 以 stdio 協議驅動 AI agent 執行修改
5. **回報結果** — 將執行結果寫回 Linear，並進行 token 用量統計與狀態追蹤

## 主要功能

- **WORKFLOW.md 解析** — 支援 YAML front matter，定義 tracker、polling、workspace、agent 等設定
- **型別化設定** — 設定值有預設值、支援環境變數間接引用、dispatch 驗證
- **Linear 整合** — GraphQL client，支援 issue 查詢、狀態更新、動態工具呼叫
- **工作區管理** — 自動建立/清理工作目錄，含路徑安全檢查與 lifecycle hooks
- **Codex 整合** — 透過 stdio 與 Codex app-server 溝通
- **Agent Runner** — 支援多輪對話（continuation turns）與 `linear_graphql` 動態工具
- **Orchestrator** — 輪詢、任務認領、重試、狀態調和、token 統計
- **CLI 命令** — `validate`、`snapshot`、`once`、`serve` 四種執行模式

## 快速開始

```bash
# 驗證 WORKFLOW.md 設定是否正確
cargo run -- validate

# 輸出目前狀態快照
cargo run -- snapshot

# 執行一次 dispatch（處理一輪 issue 後結束）
cargo run -- once

# 啟動長時間運行的服務
cargo run -- serve --i-understand-that-this-will-be-running-without-the-usual-guardrails
```

如果要連接 Linear，需要設定 API key：

```bash
export LINEAR_API_KEY=your_api_key_here
```

並在 `WORKFLOW.md` 中設定 `tracker.project_slug`。

## 專案結構

| 檔案 | 說明 |
|------|------|
| `src/workflow.rs` | WORKFLOW.md 解析器 |
| `src/config.rs` | 型別化設定與預設值 |
| `src/linear.rs` | Linear GraphQL 整合 |
| `src/workspace.rs` | 工作區建立、清理、hooks |
| `src/codex.rs` | Codex app-server client |
| `src/runner.rs` | 單一 issue 的 agent 執行器 |
| `src/orchestrator.rs` | Dispatch 排程、重試、狀態管理 |
| `src/service.rs` | 長時間運行的服務主迴圈 |

## 測試

```bash
cargo test
```

## 授權

本專案為 OpenAI Symphony 規格的獨立 Rust 實作。
