# Release Changelog

每個新版本須在建立 tag **之前**，提交 UTF-8 Markdown `changelog/v<version>.md`，例如 `changelog/v0.4.12.md`。檔案必須存在於該 tag、非空，且不可透過符號連結跳脫 `changelog/`。請填寫實際變更，不要用空白檔案或捏造歷史記事通過驗證。

`Release` workflow 的 `verify-versions` 會檢查記事，失敗時阻止後續附件與 npm 工作。`Upload release changelog` 從同一 tag 取檔，原樣上傳為該 Release 的 `changelog.md`，重跑只覆寫這個附件；Release body 保留原有內容。

## 正式發布與索引

1. 等待 Release workflow 全部成功，確認 Draft 的 `changelog.md` 與安裝資產齊全。
2. 在 GitHub UI 人工 Publish Release。`Release index` workflow 監聽 `release.published`，讀取目前完整分頁清單，排除 Draft 和 Pre-release。
3. 索引依 `published_at` 由新到舊最多保留 50 筆；`latest` 取 GitHub `/releases/latest` 的選擇。如果它在前 50 筆之外，以它替換第 50 筆再排序，確保清冊包含 latest。相同日期以 Release ID 由大到小排序。
4. `releases.json` 只上傳到 GitHub 當前 latest 正式版。舊版後補發布不會自動成為上傳目標；以 GitHub 的 latest 設定為準。不刪除任何舊 Release 或其記事。

沒有正式版時，workflow 成功結束且不上傳索引。API、資料驗證或上傳失敗會讓工作失敗，不會用不完整清單繼續上傳。所有重建工作共用 concurrency group，避免同時覆寫；上傳前再確認 latest，若變動則失敗並要求重跑。人工變更 Release 不受 concurrency 鎖定；若發布或 latest 設定恰在上傳時改變，請手動重建。

## 手動重建與限制

在 Actions → **Release index** → **Run workflow** 選預設分支，或執行：

```sh
gh workflow run release-index.yml --ref <default-branch>
```

需先把 workflow 與腳本合併到預設分支；索引 workflow 一律使用預設分支的腳本，支援舊版後補發布。Pre-release 的 published 事件也會重建，但不會將該版本列入。

`GITHUB_TOKEN` 產生的 Release 事件通常不會觸發另一條 workflow。目前流程只建立 Draft，人工 Publish 才會觸發索引。若日後使用 token 自動 Publish，應明確 dispatch 索引 workflow，不可依賴連鎖事件。GitHub UI 可繞過 CI 手動發布；workflow 無法完全封鎖這種行為。請遵守上述發布順序。歷史版本缺少記事時不會補造，也不會從索引隱藏。

如需修改 latest 設定、刪除、撤回發布或將 Pre-release 改為正式版，請手動重建索引；此 workflow 只自動監聽 published。`--clobber` 覆寫不是原子操作，若上傳失敗可能暫時缺少索引，修復後重跑即可。啟用 GitHub immutable releases 的版本可能拒絕附件覆寫，此時需先解決儲存庫發布設定限制。

## Official 資料契約

- 索引：`https://github.com/kaoruisaac/pedelec/releases/latest/download/releases.json`
- 個別記事：`https://github.com/kaoruisaac/pedelec/releases/download/{tag}/changelog.md`
- JSON：`{ "schemaVersion": 1, "latest": "vX.Y.Z", "releases": [{ "version": "vX.Y.Z", "publishedAt": "2026-11-01T00:00:00Z", "title": "Pedelec vX.Y.Z" }] }`
- 保留完整 `v` tag；日期為 UTC ISO 8601。最近 50 筆以外仍可按合法 tag 直接讀取記事。
- GitHub 附件不保證瀏覽器 fetch 所需 CORS header；Official 應由 Cloudflare Pages Function 代理讀取。Release body 不是資料權威。

## 本地驗證

```sh
node --test .github/scripts/release-metadata.test.mjs
```

測試涵蓋 0/1/50/51 筆、latest 與日期排序不同、Draft/Pre-release、舊版後補、分頁、重跑、API/上傳錯誤、latest 競態與記事驗證。測試使用模擬 API，不會發布 Release。

GitHub 端驗收需在獲授權的測試倉庫確認：人工 Publish Draft 觸發 workflow、手動 dispatch 能修復索引，以及下載 `changelog.md` 與 tag 內原始檔逐位元一致。本次實作不推 tag、不正式 Publish、不部署官網；不變更 `latest.json`、平台 bundle、簽署、SDK/npm 或 tag 觸發規則。

參考：[GitHub Release 事件](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#release)、[GITHUB_TOKEN 事件限制](https://docs.github.com/en/actions/how-tos/write-workflows/choose-when-workflows-run/trigger-a-workflow)、[Releases API](https://docs.github.com/en/rest/releases/releases)。
