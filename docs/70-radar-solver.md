# 70 · 雷達全域定位 solver（忠實移植規格）

> **這一份是特例。** 其餘 docs 是 clean-room 規格（不抄任何實作）；但這個全域定位 solver 是**專案擁有者
> 自己撰寫、且在真實伺服器上驗證過的自足數值演算法**，經授權**忠實移植**（語言直譯不違反淨室——它是
> 擁有者的 IP，不是繼承來的積垢）。**照本檔 1:1 移植到 Rust，純運算、零外部數學套件（docs 90 §8）。**
> 現有的切平面 `radar::solve` 是**錯的**（切平面在地球尺度畸變數十公里、GN 不收斂）——**本 solver 取代它**。
>
> 為什麼要移植而非重寫：數值演算法一旦降級成摘要就會丟失決定成敗的細節（切平面用在全域就是這樣爆的）。
> 忠實移植 = 零重新發明風險。

---

## 0. 一句話流程

送出 **12 個地球尺度錨點座標** → 收各自「離目標距離」→ **3D 線性閉式初估**（全域、無切平面）→
**測地線 pattern search** 粗定位 → **LM 局部精修**（此時已近目標、切平面才有效）→ 若沒命中，繞估計點
**採樣圈**再解、必要時**補充圈**、最後**無界棋盤格重試**。每次送出都回查是否已在範圍內（命中即簽到）。

---

## 1. Wire 契約（雷達答案 / 距離 / beacon / lite）

- **端點**：`PUT /api/rollcall/{id}/answer?api_version=1.76`
- **座標答案 body**（每個錨點/採樣點/估計點都送這個）：
  ```json
  { "deviceId": "<device_id>", "latitude": <lat>, "longitude": <lon>,
    "accuracy": 60, "speed": null, "heading": null, "altitude": 0, "altitudeAccuracy": null }
  ```
  `use_beacon` 時additionally附 `"radarSignal": "<sig>"`（見下）。
- **空答主路**（不變）：`PUT .../answer?api_version=1.76` body `{}`——多數雷達點名這樣就過；本 solver 只在
  空答不過（強制定位）時啟動。
- **距離萃取**（關鍵——不是頂層扁平）：對回應做**巢狀走訪**取距離：
  - 走訪順序：本體；若 dict，依序進 keys `data, result, error, errors, scope, rollcall`；若 list，取前 3 個元素。
  - 在走訪到的每個 dict 上，依序找 keys `distance, scope_distance, distance_meters, distanceMeters`，
    第一個能轉 float 的就是距離；都沒有 → `-1.0`（無距離）。
  - HTTP 2xx（已在範圍內）→ 視為命中/距離 0 → 簽到成功。錯誤信封 `error_code=="radar_out_of_rollcall_scope"`
    才帶距離。
- **beacon 簽章**：`radarSignal = md5(beacon_nonce + deviceId + userId + ts) + "," + ts`（ts=秒級 unix 時戳字串；
  userId 用登入擷取的 user_no）。md5 用純 Rust 小 hash（`md-5` crate 或手刻——非數學依賴）。
- **lite**（`GET .../lite`）只回 `{rollcall_id, use_beacon, beacon_nonce}`——**絕不含目標座標**（solver 必須靠
  距離全域反推，不能從 lite 取座標）。

---

## 2. 常數（逐字照抄）

```
MEAN_EARTH_RADIUS_M = 6371008.8            # 平均地球半徑（solver 用球面近似的半徑）
WGS84_A = 6378137.0 ; WGS84_F = 1/298.257223563 ; WGS84_B = WGS84_A*(1-WGS84_F)
SQRT_CHI2_2D_95 = 2.4477                   # sqrt(5.991)，2 自由度 95% 卡方

# GlobalRadarSolverConfig 預設：
anchor_count = 12
bearing_count = 12                         # 採樣圈每圈點數（pattern search 內部用 16，見 §7）
standard_radii_meters   = (10000, 3000, 1000, 300, 100)
supplement_radii_meters = (300, 100, 30)
robust_f_scale_meters   = 50.0             # soft-L1 尺度
measurement_sigma_meters = 0.289
target_uncertainty_95_meters = 35.0        # 95% 不確定度低於此 → 不必補充圈
max_pattern_iterations = 220
max_lm_iterations = 60
```

---

## 3. 幾何基元（純運算）

所有點為 `GeoPoint{lat, lon}`（度）。經度一律正規化到 (-180,180]。

- **haversine 距離**（球面）：
  ```
  d = 2R·atan2( sqrt(h), sqrt(1-h) )，
  h = sin²(Δlat/2) + cos(lat1)cos(lat2)sin²(Δlon/2)，R = MEAN_EARTH_RADIUS_M
  ```
- **球面 direct point**（自 origin 沿 bearing 走 dist）：
  ```
  δ = dist/R
  lat2 = asin( sin(lat1)cos(δ) + cos(lat1)sin(δ)cos(brg) )
  lon2 = lon1 + atan2( sin(brg)sin(δ)cos(lat1), cos(δ) - sin(lat1)sin(lat2) )
  ```
- **WGS84 距離/direct**：標準 **Vincenty inverse / direct**（用上面 WGS84_A/F/B；inverse 迭代上限 100、
  收斂 tol 1e-12；**不收斂則退回 haversine / 球面 direct**——v1 註明錨點只用來形成粗估、退化可接受）。
  pattern search 的步進用 `wgs84_direct_point`（測地線）；距離殘差用 `wgs84_distance_meters`。
  （移植可先用球面版跑通，再換 Vincenty 求忠實；兩者對最終結果差 <1m，因末端有 LM 局部精修。）
- **geo↔3D 單位向量**：
  ```
  unit_from_geo(p) = ( cos(lat)cos(lon), cos(lat)sin(lon), sin(lat) )
  geo_from_unit(x,y,z): 正規化後 lat=asin(clamp(z,-1,1)), lon=atan2(y,x)
  ```
- **ENU LocalFrame**（LM 精修用；在某點建東/北切平面，單位公尺）：標準 ENU——以中心點的東、北單位向量把
  局部 (x=east_m, y=north_m) ↔ GeoPoint。small-offset 用球面近似即可（LM 只在近目標處動、位移 ≤25km）。

---

## 4. 穩健成本（soft-L1）

```
soft_l1_cost(r) = f² · ( sqrt(1 + (r/f)²) - 1 )        # f = robust_f_scale_meters = 50
robust_weight(r) = 1 / sqrt(1 + (r/f)²)
residual_i(point) = wgs84_distance_meters(point, obs_i.point) - obs_i.distance
robust_cost(point) = Σ_i soft_l1_cost( residual_i(point) )
rmse(point) = sqrt( Σ residual_i² / N )
```
（觀測 `obs = {point: GeoPoint, distance: f64}`；point 是我方送出的座標、distance 是伺服器回的離目標距離。）

---

## 5. 3D 線性閉式初估（全域，無切平面）— 核心

```
spherical_initial_estimate(observations):        # 需 ≥3 觀測，否則 None
  ATA = 3x3 零矩陣; ATb = 3-向量零
  for obs in observations:
    u = unit_from_geo(obs.point)                 # 3D 單位向量
    central_angle = clamp(obs.distance / MEAN_EARTH_RADIUS_M, 0, π)
    target_dot = cos(central_angle)              # 目標與該錨點單位向量的內積 ≈ cos(角距)
    ATb += u * target_dot
    ATA += u ⊗ u                                 # 外積累加
  solved = solve_3x3(ATA, ATb)                   # 高斯消去（部分主元；pivot<1e-14 → None）
  return geo_from_unit(solved)  (or None)
```
**這一步就是「全域 refine」**：在 3D 笛卡爾球面上線性最小平方，地球尺度天生成立。

`solve_3x3`：對 3×4 增廣矩陣做部分主元高斯消去；主元絕對值 <1e-14 回 None。
（LM 用到的 `solve_2x2`：det=a11·a22-a12²，|det|<1e-18 回 None，否則克拉瑪。）

---

## 6. best_seed（初估失敗時的後備）

```
best_seed(observations):
  candidates = global_anchor_points(12) + fibonacci_points(36)
  return argmin over candidates of robust_cost(candidate)
```
- `global_anchor_points(n)`：以**正二十面體 12 頂點**為錨（取 n 個；n>12 時補 fibonacci）。
- `fibonacci_points(n)`：黃金角 `π(3-√5)` 的 fibonacci 球面撒點 → GeoPoint。

---

## 7. pattern search（測地線座標下降，全域有效、derivative-free）

```
pattern_search(start, observations, has_initial):
  current = start ; current_cost = robust_cost(current) ; iters = 0
  bearings = [ 360·k/16 for k in 0..16 ]           # 16 個方位
  radii = pattern_radii(has_initial)                # 見下
  for radius in radii:
    improved = true ; local_steps = 0
    while improved and iters < max_pattern_iterations and local_steps < 20:
      improved = false ; local_steps++ ; iters++
      best = current ; best_cost = current_cost
      for brg in bearings:
        cand = wgs84_direct_point(current, brg, radius)
        c = robust_cost(cand)
        if c + 1e-9 < best_cost: best = cand ; best_cost = c
      if best != current: current = best ; current_cost = best_cost ; improved = true
  return current

pattern_radii(has_initial):
  if has_initial:  (50000,20000,10000,5000,2000,1000,500,200,100,50,20,10,5,2,1)     # 公尺
  else (cold):     (2000000,1000000,500000,250000,100000,50000,20000,10000,5000,2000,
                    1000,500,200,100,50,20,10,5,2,1)
```
**注意**：有初估時最粗環是 **50km**（涵蓋 3D 線性初估的誤差）；冷啟從 **2000km** 起（涵蓋全球種子間距）。

---

## 8. LM 局部精修（LocalFrame 切平面——此時已近目標才有效）

```
least_squares_refine(start, observations):
  frame = LocalFrame at start                       # ENU 切平面（公尺）
  current = LocalPoint(0,0) ; damping = 1e-3
  current_cost = robust_cost(frame.to_geo(current))
  for it in 1..=max_lm_iterations:
    geo   = frame.to_geo(current)
    r     = residuals(geo)                           # 每觀測殘差
    r_e   = residuals(frame.to_geo(current + (1,0)))  # 東 +1m 數值 Jacobian
    r_n   = residuals(frame.to_geo(current + (0,1)))  # 北 +1m
    h11=h12=h22=g1=g2=0
    for (ri, rei, rni) in zip(r, r_e, r_n):
      w = robust_weight(ri)
      j1 = rei - ri ; j2 = rni - ri
      h11 += w·j1·j1 ; h12 += w·j1·j2 ; h22 += w·j2·j2
      g1  += w·j1·ri ; g2  += w·j2·ri
    step = solve_2x2( h11 + damping·max(h11,1), h12,
                      h22 + damping·max(h22,1), -g1, -g2 )
    if step is None: break
    (sx, sy) = step ; n = hypot(sx,sy)
    if n > 25000: scale = 25000/n ; sx*=scale ; sy*=scale ; n = 25000     # 位移夾在 25km
    cand = current + (sx, sy) ; cand_cost = robust_cost(frame.to_geo(cand))
    if cand_cost <= current_cost:
      current = cand ; current_cost = cand_cost ; damping = max(damping·0.35, 1e-12)
      if n < 1e-4: break
    else:
      damping = min(damping·8, 1e12)
  return frame.to_geo(current)
```

## 9. uncertainty_95（決定要不要補充圈）

以 §8 的數值 Jacobian 累加 `h11,h12,h22`（不含 g），求 2×2 反矩陣 → 較大特徵值方向的變異數 `max_var`：
```
det = h11·h22 - h12² ; if det<=1e-18: return inf
inv = [[h22,-h12],[-h12,h11]] / det
trace = inv11 + inv22 ; spread = sqrt( (inv11-inv22)² + 4·inv12² )
max_var = (trace + spread)/2
sigma = max( measurement_sigma_meters, residual_rmse )
uncertainty_95 = sqrt(max_var) · sigma · SQRT_CHI2_2D_95
```

## 10. solve_global_radar（組裝 5→8→9）

```
solve_global_radar(observations, initial=None):
  正規化觀測（distance 須為非負數，否則 RadarGeometryError）
  seed = initial or spherical_initial_estimate(obs) or best_seed(obs)
  p1 = pattern_search(seed, obs, has_initial = (initial or spherical_initial_estimate 存在))
  p2 = least_squares_refine(p1, obs)
  rmse = rmse(p2) ; u95 = uncertainty_95(p2, rmse)
  return GlobalRadarEstimate{ point:p2, residual_rmse:rmse, uncertainty_95_meters:u95 }
```

## 11. 編排（radar_runtime 的驅動迴圈）

```
1) 送 12 個 global_anchor_points 當座標答案；每個回應→距離；收集 observations。
   任何一個直接命中（HTTP 2xx 在範圍內）→ 簽到成功、結束。
2) observations < 3 → fatal（距離不足無法求解）。
3) est = solve_global_radar(observations)   # 粗定位
4) 送 standard_sample_points(est.point)：對 standard_radii(10k/3k/1k/300/100m)、每圈 bearing_count(12) 個
   測地線點（≈60 點）；邊送邊收距離；每整圈可 adaptive 重估。任何命中→成功。
5) est = solve_global_radar(observations, initial=est.point)   # 72 點估計 + rmse + u95
   送 est.point 本身；命中→成功。
6) 若未命中且 u95 偏高 → 送 supplement_sample_points(est.point)（supplement_radii 300/100/30m，各 12 點，
   bearing 偏移半格）；再 solve(initial=est.point)、送估計點。命中→成功。
7) 仍未命中 → 無界棋盤格重試：繞 est.point 由密而疏送真實候選座標，直到命中或停止。
每次「送座標」都：PUT 座標 body → 距離萃取（§1）；HTTP 2xx→命中。
```
> **ponytail 分層**：第 1-5 步（錨點→solve→標準圈）是**核心，必移植**——足以定位。第 6-7 步（補充圈 +
> 無界棋盤格 + 限流冷卻）是**穩健加強**，可先標 `ponytail:` 上限、留 R2.5；但演算法本體（§3-§10）要一次
> 移植完整，因為它是不可分割的數值單元。

## 12. 需真伺服器/帳號驗（本檔標「needs real」處）

`?api_version=1.76`、`accuracy=60`、距離信封的確切巢狀位置、`radarSignal` 的確切串接與 ts 格式、
`user_no` 來源、`MEAN_EARTH_RADIUS_M` 是否 6371008.8——這些對真租戶才能最終確認；移植先照本檔、標註待驗。
