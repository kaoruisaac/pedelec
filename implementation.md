# macOS GitHub Actions Developer ID 簽章修正實作

## 背景

Release workflow 在 `v0.1.13` 的 macOS ARM64 job 中，已成功完成 Rust release build，但在 `desktop/scripts/stage-tauri-binaries.mjs` 簽署 `pedelec-cli` 時失敗：

```text
Signing helper binary with Developer ID signing.
***: no identity found
Command failed with exit code 1: codesign ... --sign *** .../pedelec-cli
```

失敗紀錄：

- Workflow run: https://github.com/kaoruisaac/pedelec/actions/runs/30141972035
- Job: https://github.com/kaoruisaac/pedelec/actions/runs/30141972035/job/89636861110

目前 `.github/workflows/release.yml` 只檢查 Apple secrets，並將 App Store Connect API private key 寫入 `.p8` 檔案；它沒有在執行 `tauri-action` 前，將 `APPLE_CERTIFICATE` 解碼並匯入 macOS Keychain。

`tauri-action` 執行的命令為：

```text
npm run tauri build
```

而 `desktop/package.json` 定義了：

```json
{
  "scripts": {
    "pretauri": "node scripts/stage-tauri-binaries.mjs",
    "tauri": "tauri"
  }
}
```

因此 npm 會先執行 `pretauri`，之後才真正啟動 Tauri CLI。`stage-tauri-binaries.mjs` 在 `pretauri` 階段已經會呼叫 `codesign`，但此時 Tauri CLI 尚未有機會處理 `APPLE_CERTIFICATE`，runner 的 Keychain 裡也沒有 Developer ID identity，因此出現 `no identity found`。

## 實作目標

1. 在 macOS release job 執行 `tauri-action` 前，建立暫存 Keychain。
2. 將 `APPLE_CERTIFICATE` 的 base64 `.p12` 解碼並匯入暫存 Keychain。
3. 授權 `/usr/bin/codesign` 非互動式使用憑證 private key。
4. 在 build 前確認 `APPLE_SIGNING_IDENTITY` 確實存在且可供 code signing 使用。
5. 讓 `stage-tauri-binaries.mjs` 能在 `pretauri` 階段完成三個 helper binaries 的 Developer ID + Hardened Runtime 簽章。
6. 保留既有 Tauri app、DMG 簽章與 App Store Connect API notarization 流程。
7. 不論 job 成功或失敗，都清除暫存 certificate、API key 與 Keychain。

## 非目標

- 不修改 `desktop/scripts/stage-tauri-binaries.mjs` 的 helper binary 建置或簽章邏輯。
- 不改變 Windows release 流程。
- 不改用 Apple ID / app-specific password notarization。
- 不改變 Tauri updater signing secrets。
- 不將 Apple certificate、private key、Keychain password 寫入 repository 或 artifact。

## 修改範圍

### 必須修改

- `.github/workflows/release.yml`

### 預期不需修改

- `desktop/scripts/stage-tauri-binaries.mjs`
- `desktop/tauri/tauri.conf.json`
- `desktop/package.json`

`stage-tauri-binaries.mjs` 已經會在 macOS 且存在 `APPLE_SIGNING_IDENTITY` 時使用：

```text
codesign --force --timestamp --options runtime --sign <identity>
```

目前缺少的是 build 前的 Keychain 準備，而不是 helper signing command 本身。

## Secrets 前置條件

GitHub repository 必須存在以下 Actions secrets：

| Secret | 用途 | 格式 |
| --- | --- | --- |
| `APPLE_CERTIFICATE` | Developer ID Application 憑證與 private key | 完整 `.p12` 檔案的單行 base64 |
| `APPLE_CERTIFICATE_PASSWORD` | 解開 `.p12` | 匯出 `.p12` 時設定的密碼 |
| `APPLE_SIGNING_IDENTITY` | `codesign --sign` identity | 建議使用完整 `Developer ID Application: ... (TEAM_ID)` 名稱 |
| `APPLE_API_ISSUER` | App Store Connect API notarization | Issuer ID |
| `APPLE_API_KEY` | App Store Connect API notarization | Key ID |
| `APPLE_API_KEY_PRIVATE` | App Store Connect API notarization | `.p8` 完整內容 |

本次不新增固定的 `KEYCHAIN_PASSWORD` secret。Workflow 每次執行時使用 `openssl rand` 產生一次性密碼，僅存在該 job 的 process environment 中。

### `APPLE_CERTIFICATE` 必須包含 private key

Apple Developer 網站下載的 `.cer` 本身不夠。必須在原本產生 CSR、持有 private key 的 Mac 上，從 Keychain Access 的「我的憑證」匯出為 `.p12`。

建議用以下方式產生單行 base64：

```bash
openssl base64 -A \
  -in developer-id-application.p12 \
  -out developer-id-application.base64.txt
```

將 `developer-id-application.base64.txt` 的完整內容存入 `APPLE_CERTIFICATE`。

### 確認 identity

在持有憑證的 Mac 上執行：

```bash
security find-identity -v -p codesigning
```

`APPLE_SIGNING_IDENTITY` 應與輸出中的 Developer ID Application identity 相同，例如：

```text
Developer ID Application: Example Name (ABCDE12345)
```

## 實作方案

### 1. 擴充 Apple credentials 準備步驟

將目前的：

```yaml
- name: Prepare Apple notarization API key
```

改為更符合實際責任的名稱：

```yaml
- name: Prepare Apple signing and notarization credentials
```

此步驟同時完成：

1. Apple secrets 完整性檢查。
2. `.p12` certificate 解碼。
3. 暫存 Keychain 建立與 certificate 匯入。
4. codesign private key ACL / partition list 設定。
5. signing identity 驗證。
6. App Store Connect `.p8` 建立。
7. 將 cleanup 需要的路徑寫入 `$GITHUB_ENV`。

建議完整 YAML：

```yaml
- name: Prepare Apple signing and notarization credentials
  if: runner.os == 'macOS'
  shell: bash
  env:
    APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}
    APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
    APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
    APPLE_API_ISSUER: ${{ secrets.APPLE_API_ISSUER }}
    APPLE_API_KEY: ${{ secrets.APPLE_API_KEY }}
    APPLE_API_KEY_PRIVATE: ${{ secrets.APPLE_API_KEY_PRIVATE }}
  run: |
    set -euo pipefail

    required_secrets=(
      APPLE_CERTIFICATE
      APPLE_CERTIFICATE_PASSWORD
      APPLE_SIGNING_IDENTITY
      APPLE_API_ISSUER
      APPLE_API_KEY
      APPLE_API_KEY_PRIVATE
    )
    for secret_name in "${required_secrets[@]}"; do
      if [ -z "${!secret_name}" ]; then
        echo "Missing required secret: ${secret_name}" >&2
        exit 1
      fi
    done

    certificate_path="$RUNNER_TEMP/apple-developer-id.p12"
    keychain_path="$RUNNER_TEMP/pedelec-signing.keychain-db"
    private_key_dir="$RUNNER_TEMP/private_keys"
    api_key_path="$private_key_dir/AuthKey_${APPLE_API_KEY}.p8"
    keychain_password="$(openssl rand -base64 32)"
    original_default_keychain="$(security default-keychain -d user | tr -d '\"')"

    mkdir -p "$private_key_dir"
    umask 077

    printf '%s' "$APPLE_CERTIFICATE" \
      | openssl base64 -d -A \
      > "$certificate_path"
    [ -s "$certificate_path" ] || {
      echo "Decoded Apple certificate is empty." >&2
      exit 1
    }

    security create-keychain \
      -p "$keychain_password" \
      "$keychain_path"
    security set-keychain-settings \
      -lut 21600 \
      "$keychain_path"
    security unlock-keychain \
      -p "$keychain_password" \
      "$keychain_path"
    security import "$certificate_path" \
      -k "$keychain_path" \
      -P "$APPLE_CERTIFICATE_PASSWORD" \
      -T /usr/bin/codesign
    security set-key-partition-list \
      -S apple-tool:,apple:,codesign: \
      -s \
      -k "$keychain_password" \
      "$keychain_path"

    security list-keychains -d user -s "$keychain_path"
    security default-keychain -d user -s "$keychain_path"

    if ! security find-identity \
      -v \
      -p codesigning \
      "$keychain_path" \
      | grep -Fq "$APPLE_SIGNING_IDENTITY"; then
      echo "APPLE_SIGNING_IDENTITY was not found in the imported keychain." >&2
      security find-identity -v -p codesigning "$keychain_path" >&2 || true
      exit 1
    fi

    printf '%s' "$APPLE_API_KEY_PRIVATE" > "$api_key_path"
    chmod 600 "$api_key_path"

    echo "APPLE_API_KEY_PATH=$api_key_path" >> "$GITHUB_ENV"
    echo "APPLE_CERTIFICATE_PATH=$certificate_path" >> "$GITHUB_ENV"
    echo "APPLE_KEYCHAIN_PATH=$keychain_path" >> "$GITHUB_ENV"
    echo "APPLE_ORIGINAL_DEFAULT_KEYCHAIN=$original_default_keychain" >> "$GITHUB_ENV"
```

### 2. 保留 macOS `tauri-action` 環境變數

`Build and upload macOS Tauri bundles` 仍保留目前的 Apple env：

```yaml
env:
  GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
  TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
  PEDELEC_HELPER_TARGET: ${{ matrix.helper_target }}
  APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}
  APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
  APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
  APPLE_API_ISSUER: ${{ secrets.APPLE_API_ISSUER }}
  APPLE_API_KEY: ${{ secrets.APPLE_API_KEY }}
  APPLE_API_KEY_PATH: ${{ env.APPLE_API_KEY_PATH }}
```

原因：

- `pretauri` 使用已匯入的 Keychain 與 `APPLE_SIGNING_IDENTITY` 簽 helper binaries。
- Tauri CLI 繼續使用既有 Apple certificate / identity 簽署 app 與 DMG。
- Tauri CLI 繼續使用 App Store Connect API key 送 notarization 並 staple ticket。

不應移除 `APPLE_API_KEY_PATH`，也不應將 `.p8` 內容直接設為 path。

### 3. 新增無條件 cleanup step

在 `Verify signed and notarized macOS bundles` 後加入 cleanup。必須使用 `if: always() && runner.os == 'macOS'`，確保 build、upload 或驗證失敗時仍會執行。

```yaml
- name: Clean up Apple signing credentials
  if: always() && runner.os == 'macOS'
  shell: bash
  run: |
    set +e

    if [ -n "${APPLE_ORIGINAL_DEFAULT_KEYCHAIN:-}" ] \
      && [ -e "$APPLE_ORIGINAL_DEFAULT_KEYCHAIN" ]; then
      security default-keychain \
        -d user \
        -s "$APPLE_ORIGINAL_DEFAULT_KEYCHAIN"
      security list-keychains \
        -d user \
        -s "$APPLE_ORIGINAL_DEFAULT_KEYCHAIN"
    fi

    if [ -n "${APPLE_KEYCHAIN_PATH:-}" ]; then
      security delete-keychain "$APPLE_KEYCHAIN_PATH"
    fi

    rm -f \
      "${APPLE_CERTIFICATE_PATH:-}" \
      "${APPLE_API_KEY_PATH:-}"
```

注意：cleanup step 不得依賴 certificate import step 成功完成，因此所有變數都要以 `${VAR:-}` 方式讀取。

## Workflow 順序

macOS matrix job 的相關步驟應調整為：

```text
Install desktop dependencies
  ↓
Prepare Apple signing and notarization credentials
  ├─ validate secrets
  ├─ decode .p12
  ├─ create/unlock temporary keychain
  ├─ import Developer ID certificate + private key
  ├─ authorize codesign
  ├─ verify APPLE_SIGNING_IDENTITY
  └─ write App Store Connect .p8
  ↓
Build and upload macOS Tauri bundles
  ├─ npm run tauri build
  ├─ npm lifecycle runs pretauri
  ├─ stage helper binaries
  ├─ codesign helper binaries
  ├─ Tauri signs app/DMG
  └─ Tauri notarizes and staples
  ↓
Verify signed and notarized macOS bundles
  ↓
Clean up Apple signing credentials (always)
```

## 錯誤處理要求

### Secret 缺失

準備步驟應在 build 前直接失敗，並顯示缺少的 secret 名稱：

```text
Missing required secret: APPLE_CERTIFICATE
```

不得等到 Tauri build 才以不明確錯誤中止。

### Certificate base64 無效

`openssl base64 -d -A` 應以非零 exit code 失敗；若輸出為空，也要透過 `[ -s "$certificate_path" ]` 阻止流程繼續。

### `.p12` 密碼錯誤或沒有 private key

`security import` 應失敗並中止 job。不得 fallback 成 ad-hoc signing，因為 release build 必須是 Developer ID Application 簽章。

### Identity 不匹配

即使 `.p12` 成功匯入，也必須確認 `security find-identity` 的結果包含 `APPLE_SIGNING_IDENTITY`。不匹配時在 Tauri build 前失敗，避免先花數分鐘編譯 Rust 才發現問題。

常見原因：

- `APPLE_SIGNING_IDENTITY` 名稱打錯。
- identity 使用了 `Apple Development` 或 `Apple Distribution`，而不是 `Developer ID Application`。
- `.p12` 與 identity secret 不是同一張 certificate。
- `.p12` 只有 certificate、沒有 private key。
- certificate 已過期或被撤銷。

### Keychain 權限問題

必須保留：

```bash
security set-key-partition-list \
  -S apple-tool:,apple:,codesign: \
  -s \
  -k "$keychain_password" \
  "$keychain_path"
```

否則 CI 可能在 `codesign` 存取 private key 時等待 GUI 授權或回傳 user interaction / private key access error。

## 驗證方式

### 1. 靜態檢查

確認：

- Apple credentials step 僅在 macOS 執行。
- Windows job 不讀 Apple secrets。
- `tauri-action` 前已完成 certificate import。
- cleanup step 使用 `always()`。
- workflow 沒有 `echo` certificate、password、`.p8` private key。

### 2. Certificate import log

新的 macOS job 在開始編譯前應能看到：

```text
1 valid identities found
```

且不應再看到：

```text
no identity found
```

### 3. Helper binary 簽章

`pretauri` 應依序完成：

- `pedelec-cli`
- `pedelec-agent`
- `pedelec-native-host`

每個 helper 都必須通過：

```bash
codesign --verify --strict --verbose=2 <helper-path>
```

且 `codesign -dv --verbose=4` 應包含：

```text
Authority=Developer ID Application:
flags=...runtime...
```

### 4. Tauri bundle 驗證

保留目前 `Verify signed and notarized macOS bundles` step，並確認以下命令全部成功：

```bash
codesign --verify --deep --strict --verbose=2 Pedelec.app
spctl --assess --type execute --verbose=4 Pedelec.app
xcrun stapler validate Pedelec.app
xcrun stapler validate Pedelec.dmg
spctl --assess --type open --context context:primary-signature --verbose=4 Pedelec.dmg
```

### 5. 兩種 macOS 架構

以下 matrix job 都必須成功：

- `Build Tauri - macos-arm64`
- `Build Tauri - macos-x64`

不能只驗證 Apple Silicon，因為兩個 runner 都各自建立獨立的暫存 Keychain。

### 6. Release assets

Draft release 中應至少包含兩種 macOS 架構的 app updater artifact / signature 與 DMG，且 `latest.json` 能識別：

- `darwin-aarch64`
- `darwin-x86_64`

## 測試與發布注意事項

### 不要直接 re-run `v0.1.13` 的失敗 job 當作修正驗證

舊 run `30141972035` 使用的是 tag `v0.1.13` 指向的 commit。即使 main 已加入 workflow 修正，直接 re-run 舊 job 仍會使用原本的 workflow / commit，因此還是會缺少 Keychain import。

應使用以下其中一種方式驗證：

1. 合併修正、同步下一版版本號後，建立新的 release tag，例如 `v0.1.14`。
2. 從包含修正的 branch 以 `workflow_dispatch` 執行，並確保該 branch 內所有 package / Tauri / Cargo 版本與輸入的 tag 一致。

最接近正式發布流程的驗證方式是建立新 tag。

### Draft release

目前 release 會先建立 draft。macOS 簽章或 notarization 失敗時，不應將該 draft 發布。確認所有 matrix jobs 與驗證步驟成功後再 publish。

## 安全性要求

- `.p12`、`.p8`、Keychain 僅能寫入 `$RUNNER_TEMP`。
- 寫入敏感檔案前使用 `umask 077`。
- 不得使用 repository workspace 保存 certificate 或 private key。
- 不得上傳 signing credentials 為 workflow artifact。
- 不得在 log 顯示 `APPLE_CERTIFICATE_PASSWORD`、runtime Keychain password 或 private key 內容。
- cleanup 必須在失敗時執行。
- 不要將 runtime 產生的 Keychain password 寫入 `$GITHUB_ENV`；只有 import step 需要使用它。

## 風險與對策

### 風險：Tauri 再次處理 `APPLE_CERTIFICATE`

目前 macOS action 仍會收到 `APPLE_CERTIFICATE` 與 `APPLE_CERTIFICATE_PASSWORD`。這與 Tauri 官方 CI 範例一致：先手動 import certificate，再將相同 env 傳入 Tauri build。

若後續 Tauri 版本出現重複匯入衝突，優先驗證只保留已匯入 Keychain + `APPLE_SIGNING_IDENTITY` 是否足以完成 app / DMG signing；在沒有實際衝突前，不先移除既有 env，以免影響 Tauri 的自動簽章行為。

### 風險：Keychain search list 被覆蓋

暫存 Keychain 只服務此 ephemeral GitHub runner job。Build 完成後 cleanup 會恢復原本 default keychain 並刪除暫存 Keychain。

若 runner image 後續需要同時保留其他 user keychains，可再將 `security list-keychains -d user` 的原始結果保存並於 import / cleanup 時完整恢復；目前 hosted runner 的 release job 不依賴其他 user code-signing identities。

### 風險：Secret 內容包含換行

`APPLE_CERTIFICATE` 應使用 `openssl base64 -A` 產生單行內容。`APPLE_API_KEY_PRIVATE` 則保留原始 PEM 多行內容，透過 `printf '%s'` 寫入檔案。

### 風險：憑證已撤銷或過期

`security import` 可能成功，但 `security find-identity -p codesigning` 不一定會列為 valid identity。準備步驟必須以 valid identity 查詢結果作為 build gate。

## 完成條件

- [ ] `.github/workflows/release.yml` 在 macOS build 前建立暫存 Keychain。
- [ ] `APPLE_CERTIFICATE` 成功解碼成非空 `.p12`。
- [ ] `.p12` 成功匯入，且 private key 可由 `/usr/bin/codesign` 非互動式使用。
- [ ] build 前可找到與 `APPLE_SIGNING_IDENTITY` 完全匹配的 valid code-signing identity。
- [ ] `stage-tauri-binaries.mjs` 可完成三個 helper binaries 的 Developer ID + Hardened Runtime 簽章。
- [ ] macOS ARM64 build 成功。
- [ ] macOS x64 build 成功。
- [ ] `Pedelec.app` 通過 `codesign` 與 `spctl`。
- [ ] `Pedelec.app` 與 DMG 都通過 `stapler validate`。
- [ ] notarization 使用既有 App Store Connect API key 成功。
- [ ] cleanup 在成功與失敗情況都執行。
- [ ] Windows、Chrome Extension、npm publish 流程不受影響。

## 建議實作順序

1. 修改 `.github/workflows/release.yml` 的 Apple credentials 準備步驟。
2. 加入 `.p12` decode、暫存 Keychain、certificate import 與 partition list。
3. 加入 identity preflight 驗證。
4. 保留既有 `.p8` 建立與 macOS `tauri-action` env。
5. 加入 `always()` cleanup step。
6. Review workflow，確認所有敏感資料都只存在 `$RUNNER_TEMP`。
7. 提升版本並建立新 tag 驗證完整 release。
8. 確認兩個 macOS architecture jobs、bundle verification 與 notarization 全部通過後，再發布 draft release。

## 參考資料

- Tauri macOS Code Signing：https://v2.tauri.app/distribute/sign/macos/
- GitHub Actions failed job：https://github.com/kaoruisaac/pedelec/actions/runs/30141972035/job/89636861110
