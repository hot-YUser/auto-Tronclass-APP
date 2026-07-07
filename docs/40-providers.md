# 40 · 領域：學校登錄表（Providers）

TronClass 各校是同一套 API 的不同租戶。學校支援採**邏輯與資料分離**：**程式碼裡沒有任何學校字面值**，
名單一律從一份資料 seed 生成。

## 核心設計：端點全從 `base_url` 推導

一所學校**只需要一個 `base_url`**，其餘端點全部自動推導。標準端點（觀察自真實租戶）：

```
rollcalls        = {base_url}/api/radar/rollcalls?api_version=1.1.0
current_semester = {base_url}/api/current-semester-info
courses          = {base_url}/api/my-courses?...
login            = {base_url}/…（依登入特徵分流，見 30）
```

所以**新增一所學校 = 加一個 `base_url`**，不寫任何 per-school 邏輯。

## 能力一律相同

每校能力**統一**（number / radar / qrcode / 課程探索 / teacher_rollcall … 全部相同），是**程式預設**，
**不寫進 per-school 資料**。沒有「某校特別支援某功能」這種資料——要嘛全體支援，要嘛全體不支援
（見 `90-conventions.md` 的學校平等）。

## 資料 seed 結構

一份資料檔（seed），每個學校一個區塊：

```
key      學校代號（設定裡 school 欄位填的值）
label    顯示名
base_url 唯一必填；端點由它推導
aliases  中英別名（供使用者輸入時容錯匹配）
notes    備註
```

頂部有一個**可設定的預設學校**。首次啟動把 seed 寫進使用者的設定儲存，之後**那份是唯一真實來源**；
使用者刪掉某區塊/整份即以原廠 seed 重建。想永久內建一所學校 → 編輯 seed 檔。

## 收錄範圍與刻意排除

- **只收真正的 TronClass（WisdomGarden 系）租戶。** 判準是「跑同一套 TronClass API」。
- **刻意排除**：
  - 用**其他 LMS**（如 Moodle / eClass 系）的學校——那不是 TronClass API，套不上本客戶端。
  - 特定基於部署地/政策考量而刻意不收的租戶。
- 收錄一所學校前，先用登入探測確認它確實是 TronClass API（見 `30` 的登入特徵分流）。

## 平等原則（硬規矩）

任何學校名單——code、README、UI、設定——一律「**列全或都不列**」，且**從 registry 生成**，
禁止在任何一處手打某校字面值。詳見 `90-conventions.md`。
