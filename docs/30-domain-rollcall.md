# 30 · 領域：點名（Rollcall）

TronClass 的「點名」是老師在課堂即時發起、學生端回應簽到的機制。本 doc 描述**伺服器怎麼運作**與
**客戶端要做到什麼**，作為 clean-room 實作的規格。所有路徑與行為都來自對真實伺服器的觀察。

## 登入（與學校無關，特徵分流）

點名/答題都需要一個登入後、帶 session cookie 的 HTTP session。登入流程是**單一入口，純依偵測到的
登入頁特徵分流**：

- 抓一次登入頁 → 依頁面特徵決定路徑：有無圖形驗證碼（哪一種）、首頁 SSO 探索、公有雲 email SPA、
  NetIQ NAM 之類的企業 SSO。
- **分支一律以協定/頁面特徵命名，絕不以學校命名或分支**（見 `90-conventions.md`）。同一支登入流程服務
  所有學校；換學校只換 `base_url`（見 `40-providers.md`）。
- session cookie 可快取還原、驗證有效性；失效則重登。桌面可用瀏覽器手動登入作後備（填「網址」而非
  學校代號時走這條）。圖形驗證碼可選用本地 OCR（能力旗標 `ocr_captcha`）。

## 偵測：輪詢點名清單

主迴圈輪詢：

```
GET /api/radar/rollcalls?api_version=1.1.0
```

回傳目前該使用者可見的點名。**依 status 旗標分型**（每個點名恰屬一型）：

| status 特徵 | 型別 |
|---|---|
| `is_number` | number（數字碼） |
| `is_radar` | radar（雷達/定位） |
| `is_self_registration` | self_registration（自主報到） |
| `unsupported_qrcode` | qrcode（QR） |

**自適應輪詢**：偵測到活躍點名時進入 fast window（例如 30s 視窗內 poll 0.5s）；active 時 1s；idle 5s；
啟動時 fast window 30s。常數屬設定可調。**去重**：同一 rollcall 只處理一次。

## 防假點名門檻（15%）

出手前先確認**全班簽到率達 15%**（`ATTENDANCE_RATE_GATE_PERCENT = 15.0`）。這是為了避免老師誤觸產生的
**空點名**把使用者簽進一堂根本沒在點的名。未達門檻不簽；達門檻才進入 `20-contract.md` 的點名有時限
流程（15% 通過 → 15 秒倒數 → 簽到，期間可 `SignNow`/`DeferSignIn`）。門檻可經設定/旗標關閉（進階）。

## 四型與四個學生 answer 端點

學生端**恰好有四個** answer 端點，一型一個。每型送出後都**回查確認 `on_call_fine`（已簽到）才採信**。

### 1. number（數字碼）

老師公布一個數字碼（常見 4 位），學生輸入即簽到。

- **答案端點**：`PUT /api/rollcall/{rollcall_id}/answer_number_rollcall`
  body `{ "deviceId": <隨機裝置碼>, "numberCode": "0000" }`（四位字串）。
- **取碼有兩條路**：
  - **直接取碼**：`GET /api/rollcall/{rollcall_id}/student_rollcalls`（可帶 `?action=`），讀出其中的
    `number_code`。此讀取在許多租戶上對學生可見，取到即用。
  - **試碼後備**：若上面讀不到，就對答案端點**併發嘗試 `numberCode` = `0000`–`9999`**，命中即簽。
    必須**限流**：偵測到節流即冷卻並**降低併發**（退讓而非放棄）。

送出後回查 `on_call_fine`。

### 2. radar（雷達/定位）

老師發起「雷達」點名，通常伴隨地理位置驗證。**答案端點**：`PUT /api/rollcall/{rollcall_id}/answer`。
策略鏈：

- **空答（主力）**：送**空 JSON body `{}`**（不帶座標、不帶 beacon/radarSignal、不帶 `api_version`）
  即可通過——多數租戶對雷達型不強制座標內容，空提交即簽。
- **WGS84 多點定位（備援）**：當空答不過、伺服器對「答錯」回傳**距離**時，用該距離回饋做 **WGS84 橢球上
  的多點定位**反推目標座標，再以推得的座標對同一端點提交。過程可讀
  `GET /api/rollcall/{rollcall_id}/lite` 取點名精簡資訊輔助。

  求解器要求：**純運算、零外部數學套件**（numpy/scipy 之類都不引入——見 `90`，這是打包/精簡要求）。
  穩健最小平方法多點定位；不收斂則退化為**棋盤格逐格掃描**。地圖輔助為可選。

送出後回查 `on_call_fine`。

### 3. qrcode（QR）

老師投影一個**輪換的 QR**，內含一個隨時間變化的 `data` token；學生掃碼即簽。
**學生答案端點**：`PUT /api/rollcall/{rollcall_id}/answer_qr_rollcall`（body 帶 `deviceId` 與掃到的 `data`）。

- **唯一的自動路徑＝教師輔助**：用一個**獨立的教師 session** 自行起一場 QR 點名，經
  `GET /api/course/{course_id}/rollcall/{rollcall_id}/qr_code` 讀出輪換的 `data`，代學生送上面那個答案端點，
  並在確認窗口內反覆刷新重送，最後關掉教師端。需要配置教師帳號（能力旗標 `qr_teacher_assist`）；
  無教師帳號則此型**不自動化**（number/radar 不受影響）。
- **純學生端偽造 `data` 尚未被發現**（不是「不可能」）。完整的逆向研究與負面結果地圖見 `32-domain-qr.md`。
  任何手動貼上/掃描輔助都必須誠實標為「手動」，**永不包裝成自動**。

### 4. self_registration（自主報到）

最單純的一型：`PUT /api/rollcall/{rollcall_id}/answer_self_registration_rollcall`，送**空 body `{}`** 即完成報到。

> 註：某些測試租戶未開通 self_registration 服務；此型的契約已知且可離線測，實機驗證視租戶而定。

## 教師端管理端點（發起 / 開啟 / 關閉 / 讀碼）

持**教師帳號**時可自行管理一場點名（QR 教師輔助即靠這組；學生帳號對這些回 403）：

- **發起**：`POST /api/course/{course_id}/rollcall`，body 為點名設定（type、number_code、座標、時長…）→ 200/201。
- **開啟**：`POST /api/rollcall/{rollcall_id}/start-rollcall`，body 可選（如 `{ "duration": <秒> }`）→ 200/204。
- **讀 QR 輪換 data**：`GET /api/course/{course_id}/rollcall/{rollcall_id}/qr_code`。
- **讀名冊/狀態**：`GET /api/rollcall/{rollcall_id}/student_rollcalls`。
- **關閉**（依型別，皆 PUT）：
  - number → `PUT /api/rollcall/{id}/stop_number_rollcall`
  - radar → `PUT /api/rollcall/{id}/stop_radar?api_version=1.1.0`
  - self_registration → `PUT /api/rollcall/{id}/stop_time_table_rollcall`
  - qr → `PUT /api/rollcall/{id}/stop_qr_rollcall`

## 教師可直接改簽到狀態（proxy 出席）

持教師帳號者也可直接把學生標記為已簽到（`on_call_fine`）——透過該點名的 `student_rollcalls` 資源送出
（預填的 POST／帶完整物件的 PUT）。這是 QR 教師輔助之外，另一條「教師身分直接完成出席」的能力，
屬 proxy 出席。純學生帳號無此權限（403）。

## 群組

支援「一人讀碼、全員簽到」的群組模式：以一個帳號取得碼/答案後，替群組內多個 profile 一併簽到。

## 送出後的鐵律

**每一型送出後都回查確認 `on_call_fine` 才發 `SignedIn`。** 不回查就採信＝假陽性風險。

## 端點總表（Rollcall endpoint reference）

`{base}` = 學校 `base_url`（見 `40-providers.md`）。

| 用途 | 方法 | 路徑 | 備註 |
|---|---|---|---|
| 輪詢點名 | GET | `/api/radar/rollcalls?api_version=1.1.0` | 分型來源 |
| number 簽到 | PUT | `/api/rollcall/{id}/answer_number_rollcall` | body `{deviceId, numberCode:"0000"}` |
| radar 簽到 | PUT | `/api/rollcall/{id}/answer` | 空 body `{}`；備援帶座標 |
| qr 簽到 | PUT | `/api/rollcall/{id}/answer_qr_rollcall` | body `{deviceId, data}` |
| self_registration 簽到 | PUT | `/api/rollcall/{id}/answer_self_registration_rollcall` | 空 body `{}` |
| 讀名冊/碼/狀態 | GET | `/api/rollcall/{id}/student_rollcalls` | 可帶 `?action=`；含 `number_code`、`on_call_fine` |
| 讀作答彙總 | GET | `/api/rollcall/{id}/answers` | 全班簽到率（15% 門檻用） |
| radar 精簡資訊 | GET | `/api/rollcall/{id}/lite` | 解算輔助 |
| 教師發起 | POST | `/api/course/{cid}/rollcall` | 教師帳號 |
| 教師開啟 | POST | `/api/rollcall/{id}/start-rollcall` | 教師帳號 |
| 教師讀 QR data | GET | `/api/course/{cid}/rollcall/{id}/qr_code` | 教師帳號 |
| 教師關閉 | PUT | `/api/rollcall/{id}/stop_{number\|radar\|time_table\|qr}_rollcall` | radar 帶 `?api_version=1.1.0` |
