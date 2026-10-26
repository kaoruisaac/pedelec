# Pedelec Runtime 長期遷移方向：Bun 與 Sandbox 選型觀察

- **會議日期**：2026-10-15
- **文件類型**：技術方向會議記錄（非實作需求、非採用決議）
- **狀態**：持續觀察，預計 2027 年 2 月重新評估
- **討論範圍**：Pedelec 自身的 JS/TS Runtime 與執行沙箱；不包含 Codex、Claude 等 Agent Provider 自身的隔離實作

## 1. 背景與目標

Pedelec 規劃約在未來一年內逐步讓 Deno 退出自身 Runtime 架構，因此提前研究以 **Bun** 取代 Deno 的可行性，以及配套的安全隔離機制。

初步候選組合為 **Bun + Anthropic Sandbox Runtime（SRT，`@anthropic-ai/sandbox-runtime`）**。本次只是建立研究方向，並未決定正式採用 SRT，也沒有啟動遷移工程。

主要目標：

- 保留 Pedelec 的 workspace 執行、JS/TS module、工具呼叫與多工作階段能力。
- 將 Runtime 的執行與 Sandbox 的安全隔離視為兩個可獨立演進的責任。
- 盡量維持現有 SDK 對外能力與預期行為，避免因更換 Runtime 導致不必要的 API 變更。
- 在維持安全與穩定的前提下，降低 Runtime 分發、啟動及長期維護成本。

## 2. 本次討論的方向與共識

1. **Bun 是長期 Runtime 遷移候選方向**，但正式替換 Deno 的時點及方案尚未定案。
2. **SRT 僅是目前的參考選項，不是指定或已核准的 Sandbox 方案。** 以 2026 年 10 月的成熟度來看，SRT 風險偏高，不適合 Pedelec 直接投入正式環境使用。
3. **持續觀察其他 Sandbox Runtime／現成隔離方案。** 到 2027 年 2 月時，應重新比較市場上可用的選項，不能預設 SRT 一定是最佳解。
4. **不打算自行拼裝、封裝或長期維護 OS-level Sandbox。** Pedelec 可使用成熟方案底層提供的 OS 隔離能力，但不希望自己承擔 Windows／macOS／Linux 各平台 Sandbox 安全模型、修補、穩定性與相容性的主要維護責任；這不符合專案可投入的維護心力。
5. **安全性與可維護性優先於採用新技術。** 如果當時沒有足夠成熟、符合 Pedelec 情境的方案，應繼續使用既有架構、延後 Sandbox 切換，或重新評估 Runtime 遷移策略，不因時程而硬上。
6. **2027 年 2 月重新討論**可行性、成熟度、實際候選方案與是否值得啟動 PoC；屆時再決定有無必要進入實作規劃。

## 3. 初步可行性評估

Bun 在 JS/TS 執行能力上具備取代 Deno 的潛力，但 Pedelec 目前有 Runtime Owner、`workspace.run()`、Deno 專用 helper、`denoModules`、`defineDenoModule`、Vite bundling 與 Runtime 安裝管理等整合點，不能假設更換執行檔即可完成遷移。

理想架構概念：

```text
Pedelec SDK / Workspace API
            |
     Runtime Adapter
            |
    Sandbox Provider  (候選方案可替換)
            |
           Bun
            |
         Host OS
```

此圖僅表示責任分離，不代表已核准新增任何特定公開 API 或抽象層實作。

### 2026-10-24 補充候選：Microsoft eXecution Container（MXC）

- **預選狀態**：將 **Bun + Microsoft MXC** 納入與 **Bun + Anthropic SRT** 並列的候選組合；**不是採用決議，也不啟動實作或 PoC**。維持原訂 2027 年 2 月重新評估。
- **定位**：MXC 是供應用程式嵌入的開源 Sandbox SDK，以統一的權限政策及執行 API 封裝各平台的隔離後端；Pedelec 只考慮使用其 **本機 Process Sandbox** 能力，不以 VM／雲端多租戶執行環境為目標。
- **平台後端（官方預設）**：Windows 11 使用 ProcessContainer、macOS 使用 Seatbelt、Linux 使用 Bubblewrap。這代表跨平台有統一 SDK，但底層安全邊界、OS 版本門檻與行為仍須分別驗證。
- **權限與程序能力**：官方提供檔案系統讀寫／拒絕政策、網路政策、程序啟動、stdin/stdout/stderr 串流、PTY（依後端能力）、Lifecycle 與診斷介面；尚未驗證是否完全符合 Pedelec 的 ACP、多 Agent、子程序樹與 Cancel／Shutdown 需求。
- **隨產品分發**：官方 Node／.NET SDK 套件包含原生 Runtime Assets；Rust crate 可把 SDK、Engine 與選定後端編入應用程式；亦有平台專用 Executor。原則上可隨 Pedelec 安裝／發布，但需查證平台前置條件、簽章、公證、原生二進位發行與更新成本，**不能假設所有 OS 都能零前置依賴安裝**。
- **與 Bun 整合的預選方向**：官方 Node SDK 要求 Node.js 24+，**未承諾 Bun 相容**。可比較（A）Bun 直接呼叫 SDK（須實測相容性）、（B）Rust SDK 製作隨產品附帶的輕量 Launcher、（C）平台 Executor。**Rust Launcher 只作為 MXC Adapter／程序啟動器，不代表 Pedelec 自行實作或維護 OS-level Sandbox。**
- **必要驗證**：Windows 版本與 ProcessContainer 可用性；macOS Seatbelt 發布限制；Linux Bubblewrap／User Namespace 前置需求；檔案與網路 Policy 的拒絕測試；Agent 子程序是否繼承隔離；stdio/PTY/長時間 ACP 通訊；多 Workspace 並行、清理、啟動延遲與版本升級；MXC 無法啟動時必須 Fail Closed，不得默默降為 Full Access。
- **2027 年 2 月評估標準**：除了 SRT，也將 MXC 的成熟度、安全維護責任、跨平台部署、Bun 相容性及長期維護負擔列入比較；若仍不符合必要條件，維持延後遷移的選項。

官方參考：[MXC Repository](https://github.com/microsoft/mxc)、[Node SDK](https://github.com/microsoft/mxc/tree/main/sdk/node)、[Releases](https://github.com/microsoft/mxc/releases)。

## 4. 主要風險

| 面向 | 風險與待驗證事項 |
| --- | --- |
| Sandbox 成熟度 | SRT 仍新；尤其 Windows 的功能完整度、部署穩定性及問題修復速度需持續觀察。 |
| 安全邊界 | Bun + 外部 Sandbox 不會天然等同 Deno 的 permissions；要驗證讀寫、網路、環境變數、子程序及其他能力能否維持適當限制。 |
| 跨平台一致性 | Windows／macOS／Linux 依賴不同底層隔離機制，不能只在單一 OS 測試成功就視為可正式使用。 |
| 維護責任 | 避免依賴需要 Pedelec 自己持續修補 OS 安全細節、追蹤系統更新或經常介入維運的方案。 |
| 多工作區並行 | 多 workspace、session、agent 同時運作時的權限隔離、程序管理、生命週期與清理需實測。 |
| 相容性 | stdin、JS/TS 模組解析、npm 相容性、檔案存取、錯誤回報、Timeout／Cancel／Shutdown 等需驗證。 |
| 啟動與部署成本 | 衡量 Runtime + Sandbox 的完整冷啟動時間、額外依賴、初次安裝、更新、權限要求及跨平台發行難度。 |
| 生態與持續性 | 比較候選方案的維護活躍度、版本穩定性、安全更新、文件、授權與社群採用情況。 |

## 5. 2027 年 2 月重新評估項目

- 盤點當時 **SRT 以及其他可用的 Sandbox Runtime／現成隔離方案**，不要限定只能選 Anthropic。
- 優先篩選「已經有人持續維護、可供 Desktop 本地使用、有明確安全模型、跨平台可行」的方案。
- 特別確認 Windows 支援成熟度，以及是否有不可接受的管理員權限、部署或啟動要求。
- 釐清是否仍能維持至少不低於現有 Deno 執行策略的必要安全約束；不應無聲放寬權限。
- 確認 Bun 與 Sandbox 能否形成可部署的完整方案，以及是否額外強制依賴 Node 或其他大型 Runtime。
- 確認有無足夠低的整合成本與維護負擔，才考慮獨立 PoC。
- 若無合適方案，明確允許「繼續觀察／延後遷移／改選其他策略」，不必強行照原時程實施。

### 方案篩選原則

**必要條件**：可供 Pedelec 本地 Desktop 架構使用、具可驗證的安全隔離、核心平台支援合理、由外部專案持續維護、不需 Pedelec 自行打造 OS-level Sandbox。

**加分條件**：與 Bun 協作容易、啟動時間短、無沉重額外依賴、權限設定直覺、能支援多 workspace 並行且有清楚的診斷能力。

## 6. 暫不進行的事項

- 不立即更換現有 Deno Runtime。
- 不立刻將 SRT 整合進正式產品。
- 不自行建置跨平台 OS-level Sandbox 框架。
- 不提前承諾特定 Sandbox Provider、實作細節、遷移版本或上線日期。

## 7. 下次討論的預期產出

2027 年 2 月重新評估時，先輸出候選方案比較、安全／維護風險與建議取捨；若找到成熟、低維護成本的選擇，再決定是否撰寫 PoC／實作需求。

**結論**：Bun 遷移方向值得保留；Sandbox 必須開放選型。現階段 SRT 風險過高，且自行維護 OS-level 隔離不符合 Pedelec 的資源配置，因此先觀察，不啟動實作。
