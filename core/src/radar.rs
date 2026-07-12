//! Radar-rollcall global position solver — a faithful 1:1 port of `docs/70-radar-solver.md`
//! (owner-authored, server-verified numerical algorithm; sanctioned transcription). Send earth-scale
//! anchor coordinates → collect each "distance to target" → **3D linear closed-form initial estimate**
//! (global, no tangent plane) → **geodesic pattern search** → **LM local refine** → uncertainty.
//! Pure, zero external math deps (docs 90 §8). The old ENU tangent-plane solver was wrong at earth
//! scale (§0) — this replaces it.

use std::f64::consts::PI;

// ===== §2 constants (verbatim) =====
/// Mean Earth radius — the sphere radius the solver + `haversine` use.
pub const MEAN_EARTH_RADIUS_M: f64 = 6371008.8;
// ponytail: WGS84 ellipsoid — only used by true Vincenty inverse/direct, the faithfulness upgrade
// (§3 sanctions spherical-first this round; diff <1 m absorbed by the LM refine). R2.5.
#[allow(dead_code)]
const WGS84_A: f64 = 6378137.0;
#[allow(dead_code)]
const WGS84_F: f64 = 1.0 / 298.257223563;
#[allow(dead_code)]
const WGS84_B: f64 = WGS84_A * (1.0 - WGS84_F);
const SQRT_CHI2_2D_95: f64 = 2.4477; // sqrt(5.991), 2-DoF 95% chi-square

const ANCHOR_COUNT: usize = 12;
const BEARING_COUNT: usize = 12; // points per standard sampling ring (pattern search uses 16, §7)
const STANDARD_RADII: [f64; 5] = [10000.0, 3000.0, 1000.0, 300.0, 100.0];
// ponytail: supplement ring (§11 step 6) — deferred to R2.5.
#[allow(dead_code)]
const SUPPLEMENT_RADII: [f64; 3] = [300.0, 100.0, 30.0];
const ROBUST_F_SCALE: f64 = 50.0; // soft-L1 scale
const MEASUREMENT_SIGMA: f64 = 0.289;
// ponytail: the u95 threshold that decides whether to run the supplement ring — R2.5.
#[allow(dead_code)]
const TARGET_UNCERTAINTY_95: f64 = 35.0;
const MAX_PATTERN_ITERATIONS: u32 = 220;
const MAX_LM_ITERATIONS: u32 = 60;

// ===== types =====
/// A geographic point in degrees (§3). Longitude normalized to (-180, 180].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    pub lat: f64,
    pub lon: f64,
}

/// One observation: a coordinate we submitted, and the distance the server reported to the target.
#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub point: GeoPoint,
    pub distance: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct GlobalRadarEstimate {
    pub point: GeoPoint,
    // ponytail: rmse + u95 drive the §11 step-6 supplement-ring decision (deferred to R2.5); computed
    // now (part of the complete §10 unit) but not yet consumed by the steps 1-5 driver.
    #[allow(dead_code)]
    pub residual_rmse: f64,
    #[allow(dead_code)]
    pub uncertainty_95_meters: f64,
}

// ===== §3 geometry primitives =====

fn normalize_lon(lon: f64) -> f64 {
    let l = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if l <= -180.0 {
        l + 360.0
    } else {
        l
    }
}

/// Great-circle (haversine) distance in metres. The fake reuses this so the solver and fake agree
/// exactly — an independent known-value test guards the formula/radius itself.
pub fn haversine(a: GeoPoint, b: GeoPoint) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = (b.lat - a.lat).to_radians();
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * MEAN_EARTH_RADIUS_M * h.sqrt().atan2((1.0 - h).sqrt())
}

/// Spherical direct: from `origin`, travel `dist` metres along `bearing_deg`.
fn spherical_direct(origin: GeoPoint, bearing_deg: f64, dist: f64) -> GeoPoint {
    let delta = dist / MEAN_EARTH_RADIUS_M;
    let brg = bearing_deg.to_radians();
    let lat1 = origin.lat.to_radians();
    let lon1 = origin.lon.to_radians();
    let lat2 = (lat1.sin() * delta.cos() + lat1.cos() * delta.sin() * brg.cos()).asin();
    let lon2 = lon1 + (brg.sin() * delta.sin() * lat1.cos()).atan2(delta.cos() - lat1.sin() * lat2.sin());
    GeoPoint { lat: lat2.to_degrees(), lon: normalize_lon(lon2.to_degrees()) }
}

// ponytail: spherical stand-in for WGS84 — §3 explicitly sanctions spherical-first (diff <1 m,
// absorbed by the LM refine); true Vincenty inverse/direct is the faithfulness upgrade (R2.5).
fn wgs84_distance_meters(a: GeoPoint, b: GeoPoint) -> f64 {
    haversine(a, b)
}
fn wgs84_direct_point(origin: GeoPoint, bearing_deg: f64, dist: f64) -> GeoPoint {
    spherical_direct(origin, bearing_deg, dist)
}

fn unit_from_geo(p: GeoPoint) -> [f64; 3] {
    let (lat, lon) = (p.lat.to_radians(), p.lon.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}
fn geo_from_unit(v: [f64; 3]) -> GeoPoint {
    let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let (x, y, z) = if n > 0.0 { (v[0] / n, v[1] / n, v[2] / n) } else { (v[0], v[1], v[2]) };
    let lat = z.clamp(-1.0, 1.0).asin();
    let lon = y.atan2(x);
    GeoPoint { lat: lat.to_degrees(), lon: normalize_lon(lon.to_degrees()) }
}

/// ENU tangent frame (metres) at a center — used by the LM refine, which only moves near the target.
struct LocalFrame {
    center: GeoPoint,
    cos_lat: f64,
}
impl LocalFrame {
    fn new(center: GeoPoint) -> LocalFrame {
        LocalFrame { center, cos_lat: center.lat.to_radians().cos() }
    }
    fn to_geo(&self, east_m: f64, north_m: f64) -> GeoPoint {
        let dlat = (north_m / MEAN_EARTH_RADIUS_M).to_degrees();
        let denom = if self.cos_lat.abs() < 1e-9 { 1e-9 } else { self.cos_lat };
        let dlon = (east_m / (MEAN_EARTH_RADIUS_M * denom)).to_degrees();
        GeoPoint { lat: self.center.lat + dlat, lon: normalize_lon(self.center.lon + dlon) }
    }
}

// ===== §4 robust cost (soft-L1) =====
fn soft_l1_cost(r: f64) -> f64 {
    let f = ROBUST_F_SCALE;
    f * f * ((1.0 + (r / f).powi(2)).sqrt() - 1.0)
}
fn robust_weight(r: f64) -> f64 {
    let f = ROBUST_F_SCALE;
    1.0 / (1.0 + (r / f).powi(2)).sqrt()
}
fn residual_i(point: GeoPoint, obs: &Observation) -> f64 {
    wgs84_distance_meters(point, obs.point) - obs.distance
}
fn robust_cost(obs: &[Observation], point: GeoPoint) -> f64 {
    obs.iter().map(|o| soft_l1_cost(residual_i(point, o))).sum()
}
fn rmse(obs: &[Observation], point: GeoPoint) -> f64 {
    if obs.is_empty() {
        return f64::INFINITY;
    }
    let s: f64 = obs.iter().map(|o| residual_i(point, o).powi(2)).sum();
    (s / obs.len() as f64).sqrt()
}
fn residuals(obs: &[Observation], geo: GeoPoint) -> Vec<f64> {
    obs.iter().map(|o| residual_i(geo, o)).collect()
}

// ===== §5 3D linear closed-form initial estimate (global — the true "global refine") =====
fn spherical_initial_estimate(obs: &[Observation]) -> Option<GeoPoint> {
    if obs.len() < 3 {
        return None;
    }
    let mut ata = [[0.0f64; 3]; 3];
    let mut atb = [0.0f64; 3];
    for o in obs {
        let u = unit_from_geo(o.point);
        let central_angle = (o.distance / MEAN_EARTH_RADIUS_M).clamp(0.0, PI);
        let target_dot = central_angle.cos();
        for i in 0..3 {
            atb[i] += u[i] * target_dot;
            for j in 0..3 {
                ata[i][j] += u[i] * u[j];
            }
        }
    }
    solve_3x3(ata, atb).map(geo_from_unit)
}

/// Partial-pivot Gaussian elimination on the 3×4 augmented system; pivot <1e-14 → None.
fn solve_3x3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> Option<[f64; 3]> {
    for col in 0..3 {
        let mut piv = col;
        for r in (col + 1)..3 {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-14 {
            return None;
        }
        a.swap(col, piv);
        b.swap(col, piv);
        for r in 0..3 {
            if r == col {
                continue;
            }
            let factor = a[r][col] / a[col][col];
            #[allow(clippy::needless_range_loop)] // indexes two rows (a[r] and a[col]) — no iterator form
            for c in col..3 {
                a[r][c] -= factor * a[col][c];
            }
            b[r] -= factor * b[col];
        }
    }
    Some([b[0] / a[0][0], b[1] / a[1][1], b[2] / a[2][2]])
}

/// Symmetric 2×2 solve via Cramer; |det| <1e-18 → None.
fn solve_2x2(a11: f64, a12: f64, a22: f64, b1: f64, b2: f64) -> Option<(f64, f64)> {
    let det = a11 * a22 - a12 * a12;
    if det.abs() < 1e-18 {
        return None;
    }
    Some(((b1 * a22 - a12 * b2) / det, (a11 * b2 - b1 * a12) / det))
}

// ===== §6 best_seed fallback + anchor / fibonacci points =====
fn best_seed(obs: &[Observation]) -> Option<GeoPoint> {
    let mut candidates = global_anchor_points(ANCHOR_COUNT);
    candidates.extend(fibonacci_points(36));
    candidates
        .into_iter()
        .min_by(|a, b| robust_cost(obs, *a).partial_cmp(&robust_cost(obs, *b)).unwrap_or(std::cmp::Ordering::Equal))
}

/// `n` earth-scale anchor coordinates: the 12 icosahedron vertices, padded with fibonacci if n>12.
pub fn global_anchor_points(n: usize) -> Vec<GeoPoint> {
    let mut pts = icosahedron_vertices();
    if n <= pts.len() {
        pts.truncate(n);
    } else {
        pts.extend(fibonacci_points(n - pts.len()));
    }
    pts
}

fn icosahedron_vertices() -> Vec<GeoPoint> {
    let phi = (1.0 + 5.0_f64.sqrt()) / 2.0;
    [
        [-1.0, phi, 0.0], [1.0, phi, 0.0], [-1.0, -phi, 0.0], [1.0, -phi, 0.0],
        [0.0, -1.0, phi], [0.0, 1.0, phi], [0.0, -1.0, -phi], [0.0, 1.0, -phi],
        [phi, 0.0, -1.0], [phi, 0.0, 1.0], [-phi, 0.0, -1.0], [-phi, 0.0, 1.0],
    ]
    .iter()
    .map(|v| geo_from_unit(*v))
    .collect()
}

fn fibonacci_points(n: usize) -> Vec<GeoPoint> {
    if n == 0 {
        return vec![];
    }
    let golden = PI * (3.0 - 5.0_f64.sqrt());
    (0..n)
        .map(|i| {
            let y = 1.0 - 2.0 * (i as f64 + 0.5) / n as f64;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let theta = golden * i as f64;
            geo_from_unit([theta.cos() * r, theta.sin() * r, y])
        })
        .collect()
}

// ===== §7 pattern search (geodesic coordinate descent, global, derivative-free) =====
fn pattern_search(start: GeoPoint, obs: &[Observation], has_initial: bool) -> GeoPoint {
    let mut current = start;
    let mut current_cost = robust_cost(obs, current);
    let bearings: Vec<f64> = (0..16).map(|k| 360.0 * k as f64 / 16.0).collect();
    let mut iters = 0u32;
    for &radius in pattern_radii(has_initial) {
        let mut improved = true;
        let mut local_steps = 0u32;
        while improved && iters < MAX_PATTERN_ITERATIONS && local_steps < 20 {
            improved = false;
            local_steps += 1;
            iters += 1;
            let mut best = current;
            let mut best_cost = current_cost;
            for &brg in &bearings {
                let cand = wgs84_direct_point(current, brg, radius);
                let c = robust_cost(obs, cand);
                if c + 1e-9 < best_cost {
                    best = cand;
                    best_cost = c;
                }
            }
            if best != current {
                current = best;
                current_cost = best_cost;
                improved = true;
            }
        }
    }
    current
}

fn pattern_radii(has_initial: bool) -> &'static [f64] {
    if has_initial {
        &[50000., 20000., 10000., 5000., 2000., 1000., 500., 200., 100., 50., 20., 10., 5., 2., 1.]
    } else {
        &[
            2000000., 1000000., 500000., 250000., 100000., 50000., 20000., 10000., 5000., 2000., 1000.,
            500., 200., 100., 50., 20., 10., 5., 2., 1.,
        ]
    }
}

// ===== §8 LM local refine (ENU tangent frame — valid once near the target) =====
fn least_squares_refine(start: GeoPoint, obs: &[Observation]) -> GeoPoint {
    let frame = LocalFrame::new(start);
    let (mut cx, mut cy) = (0.0f64, 0.0f64);
    let mut damping = 1e-3f64;
    let mut current_cost = robust_cost(obs, frame.to_geo(cx, cy));
    for _ in 1..=MAX_LM_ITERATIONS {
        let r = residuals(obs, frame.to_geo(cx, cy));
        let r_e = residuals(obs, frame.to_geo(cx + 1.0, cy));
        let r_n = residuals(obs, frame.to_geo(cx, cy + 1.0));
        let (mut h11, mut h12, mut h22, mut g1, mut g2) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for i in 0..r.len() {
            let w = robust_weight(r[i]);
            let j1 = r_e[i] - r[i];
            let j2 = r_n[i] - r[i];
            h11 += w * j1 * j1;
            h12 += w * j1 * j2;
            h22 += w * j2 * j2;
            g1 += w * j1 * r[i];
            g2 += w * j2 * r[i];
        }
        let Some((mut sx, mut sy)) =
            solve_2x2(h11 + damping * h11.max(1.0), h12, h22 + damping * h22.max(1.0), -g1, -g2)
        else {
            break;
        };
        let mut n = sx.hypot(sy);
        if n > 25000.0 {
            let scale = 25000.0 / n;
            sx *= scale;
            sy *= scale;
            n = 25000.0;
        }
        let cand_cost = robust_cost(obs, frame.to_geo(cx + sx, cy + sy));
        if cand_cost <= current_cost {
            cx += sx;
            cy += sy;
            current_cost = cand_cost;
            damping = (damping * 0.35).max(1e-12);
            if n < 1e-4 {
                break;
            }
        } else {
            damping = (damping * 8.0).min(1e12);
        }
    }
    frame.to_geo(cx, cy)
}

// ===== §9 uncertainty_95 =====
fn uncertainty_95(obs: &[Observation], point: GeoPoint, residual_rmse: f64) -> f64 {
    let frame = LocalFrame::new(point);
    let r = residuals(obs, frame.to_geo(0.0, 0.0));
    let r_e = residuals(obs, frame.to_geo(1.0, 0.0));
    let r_n = residuals(obs, frame.to_geo(0.0, 1.0));
    let (mut h11, mut h12, mut h22) = (0.0, 0.0, 0.0);
    for i in 0..r.len() {
        let w = robust_weight(r[i]);
        let j1 = r_e[i] - r[i];
        let j2 = r_n[i] - r[i];
        h11 += w * j1 * j1;
        h12 += w * j1 * j2;
        h22 += w * j2 * j2;
    }
    let det = h11 * h22 - h12 * h12;
    if det <= 1e-18 {
        return f64::INFINITY;
    }
    let (inv11, inv12, inv22) = (h22 / det, -h12 / det, h11 / det);
    let trace = inv11 + inv22;
    let spread = ((inv11 - inv22).powi(2) + 4.0 * inv12 * inv12).sqrt();
    let max_var = (trace + spread) / 2.0;
    let sigma = MEASUREMENT_SIGMA.max(residual_rmse);
    max_var.sqrt() * sigma * SQRT_CHI2_2D_95
}

// ===== §10 solve_global_radar (assemble 5 → 7 → 8 → 9) =====
pub fn solve_global_radar(obs: &[Observation], initial: Option<GeoPoint>) -> Option<GlobalRadarEstimate> {
    if obs.len() < 3 || obs.iter().any(|o| o.distance < 0.0) {
        return None;
    }
    let spherical = spherical_initial_estimate(obs);
    let has_initial = initial.is_some() || spherical.is_some();
    let seed = initial.or(spherical).or_else(|| best_seed(obs))?;
    let p1 = pattern_search(seed, obs, has_initial);
    let p2 = least_squares_refine(p1, obs);
    let e = rmse(obs, p2);
    let u95 = uncertainty_95(obs, p2, e);
    Some(GlobalRadarEstimate { point: p2, residual_rmse: e, uncertainty_95_meters: u95 })
}

// ===== §11 driver helper: the standard sampling rings around an estimate =====
pub fn standard_sample_points(center: GeoPoint) -> Vec<GeoPoint> {
    let mut pts = Vec::with_capacity(STANDARD_RADII.len() * BEARING_COUNT);
    for &radius in &STANDARD_RADII {
        for k in 0..BEARING_COUNT {
            let brg = 360.0 * k as f64 / BEARING_COUNT as f64;
            pts.push(wgs84_direct_point(center, brg, radius));
        }
    }
    pts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_matches_published_distances() {
        // Independent of the fake: guards the formula/radius (a bug here hides behind the self-
        // consistent fake but offsets a real, true-geodesic server).
        let paris = GeoPoint { lat: 48.8566, lon: 2.3522 };
        let tokyo = GeoPoint { lat: 35.6762, lon: 139.6503 };
        let d = haversine(paris, tokyo); // published great-circle ≈ 9713 km
        assert!((d - 9_713_000.0).abs() < 40_000.0, "Paris-Tokyo ≈ 9713 km, got {:.0} m", d);
        let one_deg = haversine(GeoPoint { lat: 0.0, lon: 0.0 }, GeoPoint { lat: 1.0, lon: 0.0 });
        assert!((one_deg - 111_195.0).abs() < 300.0, "1° lat ≈ 111.19 km, got {:.1} m", one_deg);
    }

    #[test]
    fn solve_global_reverses_a_hidden_target() {
        let target = GeoPoint { lat: 48.8584, lon: 2.2945 }; // Eiffel — far from the old (25, 121.5)
        let anchors = global_anchor_points(ANCHOR_COUNT);
        let obs: Vec<Observation> =
            anchors.iter().map(|&a| Observation { point: a, distance: haversine(a, target) }).collect();
        let est = solve_global_radar(&obs, None).expect("solved").point;
        assert!(haversine(est, target) < 10.0, "global reverse <10 m, got {:.2}", haversine(est, target));

        // Add near ring observations → the LM refine lands <1 m.
        let mut obs2 = obs.clone();
        for &radius in &[1000.0, 300.0, 100.0] {
            for k in 0..BEARING_COUNT {
                let p = spherical_direct(est, 360.0 * k as f64 / BEARING_COUNT as f64, radius);
                obs2.push(Observation { point: p, distance: haversine(p, target) });
            }
        }
        let est2 = solve_global_radar(&obs2, Some(est)).expect("refined").point;
        assert!(haversine(est2, target) < 1.0, "LM refine <1 m, got {:.3}", haversine(est2, target));
    }

    #[test]
    fn too_few_observations_is_none() {
        let obs = [
            Observation { point: GeoPoint { lat: 25.0, lon: 121.0 }, distance: 100.0 },
            Observation { point: GeoPoint { lat: 25.1, lon: 121.1 }, distance: 100.0 },
        ];
        assert!(solve_global_radar(&obs, None).is_none());
    }
}
