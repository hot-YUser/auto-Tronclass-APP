//! Radar-rollcall position solver: given several `(lat, lon, distance)` observations, recover the
//! target coordinate. **Pure, zero external math deps** (docs 90 §8) — a small ENU tangent-plane
//! projection + Gauss–Newton least squares, with a grid-search fallback when it doesn't converge.
//! Campus-scale (<~1 km) so the spherical projection error is negligible.

const EARTH_RADIUS_M: f64 = 6_371_000.0;

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub lat: f64,
    pub lon: f64,
    pub dist_m: f64,
}

/// Solve for the target `(lat, lon)`. Needs ≥3 non-degenerate observations.
pub fn solve(obs: &[Observation]) -> Option<(f64, f64)> {
    if obs.len() < 3 {
        return None;
    }
    let n = obs.len() as f64;
    let lat0 = obs.iter().map(|o| o.lat).sum::<f64>() / n;
    let lon0 = obs.iter().map(|o| o.lon).sum::<f64>() / n;

    // Project anchors into a local east/north metre frame centred on the centroid.
    let anchors: Vec<(f64, f64, f64)> =
        obs.iter().map(|o| { let (x, y) = to_local(o.lat, o.lon, lat0, lon0); (x, y, o.dist_m) }).collect();

    let mut px = anchors.iter().map(|a| a.0).sum::<f64>() / n;
    let mut py = anchors.iter().map(|a| a.1).sum::<f64>() / n;

    // Gauss–Newton on residual r_i = ||p - a_i|| - d_i.
    for _ in 0..100 {
        let (mut jtj00, mut jtj01, mut jtj11) = (0.0f64, 0.0f64, 0.0f64);
        let (mut jtr0, mut jtr1) = (0.0f64, 0.0f64);
        for &(ax, ay, d) in &anchors {
            let dx = px - ax;
            let dy = py - ay;
            let range = (dx * dx + dy * dy).sqrt().max(1e-6);
            let r = range - d;
            let (jx, jy) = (dx / range, dy / range);
            jtj00 += jx * jx;
            jtj01 += jx * jy;
            jtj11 += jy * jy;
            jtr0 += jx * r;
            jtr1 += jy * r;
        }
        let det = jtj00 * jtj11 - jtj01 * jtj01;
        if det.abs() < 1e-9 {
            break;
        }
        let step_x = -(jtj11 * jtr0 - jtj01 * jtr1) / det;
        let step_y = -(-jtj01 * jtr0 + jtj00 * jtr1) / det;
        px += step_x;
        py += step_y;
        if step_x.hypot(step_y) < 1e-4 {
            break;
        }
    }

    // If the least-squares fit is poor (bad geometry / local minimum), fall back to a grid scan.
    let rms = rms_residual(&anchors, px, py);
    if !rms.is_finite() || rms > 50.0 {
        if let Some((gx, gy)) = grid_search(&anchors) {
            px = gx;
            py = gy;
        }
    }

    Some(from_local(px, py, lat0, lon0))
}

fn to_local(lat: f64, lon: f64, lat0: f64, lon0: f64) -> (f64, f64) {
    let east = (lon - lon0).to_radians() * EARTH_RADIUS_M * lat0.to_radians().cos();
    let north = (lat - lat0).to_radians() * EARTH_RADIUS_M;
    (east, north)
}

fn from_local(x: f64, y: f64, lat0: f64, lon0: f64) -> (f64, f64) {
    let lat = lat0 + (y / EARTH_RADIUS_M).to_degrees();
    let lon = lon0 + (x / (EARTH_RADIUS_M * lat0.to_radians().cos())).to_degrees();
    (lat, lon)
}

fn rms_residual(anchors: &[(f64, f64, f64)], px: f64, py: f64) -> f64 {
    let n = anchors.len() as f64;
    let sum: f64 = anchors.iter().map(|&(ax, ay, d)| { let e = (px - ax).hypot(py - ay) - d; e * e }).sum();
    (sum / n).sqrt()
}

// ponytail: coarse fixed 200×200 chessboard scan — fine as a rare fallback; refine adaptively if
// a real tenant ever needs sub-metre accuracy from bad geometry.
fn grid_search(anchors: &[(f64, f64, f64)]) -> Option<(f64, f64)> {
    let minx = anchors.iter().map(|a| a.0 - a.2).fold(f64::INFINITY, f64::min);
    let maxx = anchors.iter().map(|a| a.0 + a.2).fold(f64::NEG_INFINITY, f64::max);
    let miny = anchors.iter().map(|a| a.1 - a.2).fold(f64::INFINITY, f64::min);
    let maxy = anchors.iter().map(|a| a.1 + a.2).fold(f64::NEG_INFINITY, f64::max);
    if !(minx.is_finite() && maxx.is_finite() && miny.is_finite() && maxy.is_finite()) {
        return None;
    }
    let steps = 200;
    let mut best: Option<(f64, f64)> = None;
    let mut best_e = f64::INFINITY;
    for i in 0..=steps {
        for j in 0..=steps {
            let x = minx + (maxx - minx) * i as f64 / steps as f64;
            let y = miny + (maxy - miny) * j as f64 / steps as f64;
            let e = rms_residual(anchors, x, y);
            if e < best_e {
                best_e = e;
                best = Some((x, y));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo_dist(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
        let (x, y) = to_local(lat1, lon1, lat2, lon2);
        x.hypot(y)
    }

    #[test]
    fn recovers_a_known_target() {
        // Target on a campus; four anchors around it with exact distances.
        let (tlat, tlon) = (25.014_000, 121.535_000);
        let anchors = [
            (25.0150, 121.5340),
            (25.0130, 121.5360),
            (25.0155, 121.5365),
            (25.0125, 121.5342),
        ];
        let obs: Vec<Observation> = anchors
            .iter()
            .map(|&(lat, lon)| Observation { lat, lon, dist_m: geo_dist(lat, lon, tlat, tlon) })
            .collect();

        let (slat, slon) = solve(&obs).expect("solved");
        let err = geo_dist(slat, slon, tlat, tlon);
        assert!(err < 2.0, "recovered target within 2 m, got {err:.3} m");
    }

    #[test]
    fn too_few_observations_is_none() {
        let obs =
            [Observation { lat: 25.0, lon: 121.0, dist_m: 100.0 }, Observation { lat: 25.1, lon: 121.1, dist_m: 100.0 }];
        assert!(solve(&obs).is_none());
    }
}
