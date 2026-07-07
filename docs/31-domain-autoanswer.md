# 31 · 領域：測驗答題（Auto-answer）

最核心也最複雜的子系統。背景偵測進行中的測驗/作業 → **備答（不送）→ 給反悔窗 → 送出**。本 doc 是
TronClass 出題/作答/取正解 API 的全貌 + 逐型計分細節 + LLM 整合，全部來自對真實伺服器的實機驗證。

## 總流程

1. **偵測**進行中活動。
2. **prepare（備答，不送）**：取題（GET）→ 對每題決策 → 快取答案。
3. **等反悔窗**（預設 15 秒）→ **submit（送出）**。使用者可立即送、暫緩、或捨棄（見 `20-contract.md` 流程 A）。
4. 若活動允許重作且複閱洩漏正解 → **再送一次正解**（設定 `resubmit_for_correct`）。

**鐵律：LLM 連不上/取不到答案時，寧可略過不送，也不送空白。** 仍空白重問上限 `MAX_ANSWER_REASK = 4`，
超過寧可不送該題。

## 純決策層 vs I/O 層（可直接單測）

把「決策」與「I/O」分開：決策層（`decide_paper` 等）**零 I/O、純函式**，直接單測。
`decide_paper()`：**每題**先看有無 server 洩漏的正解——

- 有洩漏正解 → 標 `REPLAY`（直接用正解）。
- 否則 → `pending`（交給 LLM）。

**每題都拿到一個真實答案**（replay 或 LLM），不對未計分題盲猜。

## 活動來源與單一入口分流

答題入口是**單一函式、依活動型別動態分流**（與登入流程同哲學）。已知來源：

- **exam（測驗）**——主力，契約最完整（見下）。
- **courseware-quiz（隨堂/課件測驗）**——學生端端點已知，正解是否漏給學生未定論（見下）。
- **interaction / vote（投票/互動）**。
- **classroom-exam（課堂測驗）**。
- **homework（作業）**。

## 題型與分流

`QuestionType` 值＝TronClass API 的 type 字串：

```
single_selection · multiple_selection · true_or_false ·
fill_in_blank · short_answer · cloze · matching ·
media · analysis · paragraph_desc
```

- **GROUP_TYPES = (media, analysis)**：帶 `sub_subjects`，**展平成子題**逐一作答。
- **SKIP_TYPES = (paragraph_desc,)**：純敘述段，直接略過。
- **BLANK_TYPES = (fill_in_blank, cloze)**：逐格放進 submission 的 `answers` 陣列。
- **matching**：子項用 `parent_id` 綁容器，讓伺服器精確計分（細節見「計分 gotcha」）。

## exam：學生端取題/作答/取正解

- **資格檢查**：取題前 `GET /api/exam/{id}/check-exam-qualification`。
- **取卷**：`GET /api/exams/{id}/distribute` → 回 `exam_paper_instance_id` + `subjects[]`（題幹 + 選項；
  **交卷前不含 `is_answer`/正解**）。`/subjects`、`/preview` 對學生 403（teacher-only）。
- **暫存**：`POST /api/exams/{eid}/submissions/storage`。
- **交卷**：`POST /api/exams/{eid}/submissions`，body：
  ```
  { exam_paper_instance_id,
    subjects: [ { subject_id, answer_option_ids: [...], answer: "" } ] }
  ```
  → 201 `{ submission_id, allow_retake_exam }`。表單真實欄位用 camelCase `examFinished: true`。
- **複閱洩漏正解**：當 exam 設 `announce_answer=immediate` 時，
  `GET /api/exams/{eid}/submissions/{sid}` 會把正解漏給學生：`subjects_data.subjects[].options[].is_answer`
  以及 `correct_answers_data.answer_option_ids`。
- **保滿分策略**：若 `allow_retake_exam` 且 `exam_score_rule = highest`——先交任意 → 讀複閱拿正解 →
  再交正解 → 取最高分。可全自動。伺服器對逾次/重交回 **400 擋下、不覆蓋**（守門只省無謂重試）。

## 計分 gotcha（治本要點）

實機逐型核分得出兩個**送出值必須逐字正確**的坑：

1. **填空/簡答（fill_in_blank / cloze / short_answer）連 HTML 標記一起送。**
   伺服器把正解存成帶 rich-text 標籤的原文（如 `<p>巴黎</p>`）並**逐字比對（case_sensitive）**。
   送純文字「巴黎」→ 計 0；送 `<p>巴黎</p>` 原文 → 滿分。
   ⇒ **送出值一律 verbatim（replay/resubmit 用複閱原文）；HTML 只在主控台顯示層清理，別在送出前 strip。**

2. **matching 的正解在「每個左項子題自己的 `options`」。**
   學生端 distribute 時：子題 options 空、pair-options 全掛在容器上、無連結欄位、無 `is_answer`。
   要靠「id 連續切塊」把子題對回選項。**被編輯/打亂過的卷**，複閱可能漏掉別子題區塊的 id，
   直接套用會誤配計 0。⇒ resubmit 疊加正解時做**成員驗證**：複閱正解 id 只在「確為該子題真選項」時才套用，
   否則保留首答。

**核分欄位**：submission 的 `score`（總）＋ `submission_score_data`{subject_id: 分} ＋
`correct_data`{subject_id: bool}。（不是 `exam_score`，也不在 `subjects[]`。）

## vote（投票）cast 契約

- 學生投票：`POST /api/votes/{id}/vote`，body `{ "votes": ["A","C"] }`——值是**選項字母**（非 id、非文字）。
- 選項來源：`interaction.data.vote_option_items`。

## courseware-quiz：學生端端點

`GET /api/courseware-quiz/quiz/{id}/subjects`、`POST .../submissions`、`GET .../my-submission`。
送出 wrapper 為 `subjects_answers`、每題帶 `type`、文字走 `answers`。
**「學生端 /subjects 是否漏 correct_answers」尚未定論**——需要一個真實的課件測驗才能驗；此為已知的
待補資料點（不要假設漏或不漏）。

## homework / classroom-exam 細節

- homework 送出補 `slides: []`。作答次數看每生 `submission_count` + `submit_times`（無 `has_submitted`）。
- classroom-exam：`GET /api/classroom-exams/{id}`；提交 `POST /api/classroom/{id}/submit/{subjectId}`。
  狀態機只有 none/start/finish；「可送出的門」＝ `started_subjects_count ≥ 1`（"start" 為必要非充分）。

## LLM 客戶端

- **預設 NVIDIA NIM，模型 `minimax` 家族（reasoning 型）。** 供應端點/模型/金鑰皆屬設定；金鑰來自
  加密保險庫（見 `10`），**永不落明碼、永不進日誌**。
- **reasoning 常開**：以 `chat_template_kwargs` 開啟；把 reasoning 與最終答案分離，**只取乾淨的最終答案**
  送出。reasoning 串流可經 `ReasoningChunk` 事件給 UI 展開觀看（見 `20` 流程 A）。
- **必送明確 `max_tokens`。** 這類 reasoning 模型若省略 `max_tokens`，會出現「HTTP 200 但 `choices` 空」
  的回空——**開 reasoning 時務必帶足夠的 `max_tokens` 上限**。temperature 約 0.6 較穩。
- **工具呼叫讀教材**：單一工具讓模型在需要時抓課程教材/PDF 文字（PDF 抽字，import-guard；缺解析庫就降級）。
- **多模態**：需登入才看得到的題目圖，以 base64 內嵌給模型。
- 偏好/設定持久化於設定儲存。

## 顯示 vs 送出

主控台/UI 的顯示會清理 HTML 讓人看得舒服（一個純顯示用的清理函式），但**送出的值一律 verbatim**——
顯示層的清理**絕不可**污染送出值（見上「計分 gotcha 1」）。
