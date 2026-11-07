//! History chart renderer: faint gridlines + colored polylines on cairo.
//! Each series is normalized against its own observed range (the units
//! differ: °C, W, %). No text is drawn on the surface — the GTK legend
//! beside the chart carries the color mapping.

use libadwaita::gtk::cairo::Context;

const TEMP: (f64, f64, f64) = (0.86, 0.28, 0.28); // red
const POWER: (f64, f64, f64) = (0.26, 0.47, 0.87); // blue
const UTIL: (f64, f64, f64) = (0.27, 0.70, 0.35); // green

/// Draws the chart. `width`/`height` come from the GTK draw-func callback.
/// Series are `(t_seconds, value)` pairs as maintained by `SeriesData`.
pub fn draw(
    cr: &Context,
    width: i32,
    height: i32,
    cpu_temp: &[(f64, Option<f64>)],
    pkg_power: &[(f64, Option<f64>)],
    cpu_util: &[(f64, Option<f64>)],
    _start_ms: Option<i64>,
) {
    let w = width as f64;
    let h = height as f64;
    if w < 4.0 || h < 4.0 {
        return;
    }

    // subtle card background so the chart reads as a panel even in light mode
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.06);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    // horizontal gridlines: 5 evenly spaced rows
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.25);
    cr.set_line_width(1.0);
    for i in 1..5 {
        let y = h * i as f64 / 5.0;
        cr.move_to(0.0, y);
        cr.line_to(w, y);
        let _ = cr.stroke();
    }

    let Some((min_t, max_t)) = time_bounds(&[cpu_temp, pkg_power, cpu_util]) else {
        return;
    };

    stroke_series(cr, cpu_temp, (min_t, max_t), w, h, TEMP);
    stroke_series(cr, pkg_power, (min_t, max_t), w, h, POWER);
    stroke_series(cr, cpu_util, (min_t, max_t), w, h, UTIL);
}

fn time_bounds(series: &[&[(f64, Option<f64>)]]) -> Option<(f64, f64)> {
    let min = series
        .iter()
        .filter_map(|s| s.first().map(|p| p.0))
        .reduce(f64::min)?;
    let max = series
        .iter()
        .filter_map(|s| s.last().map(|p| p.0))
        .reduce(f64::max)?;
    (max > min).then_some((min, max))
}

fn stroke_series(
    cr: &Context,
    series: &[(f64, Option<f64>)],
    (min_t, max_t): (f64, f64),
    w: f64,
    h: f64,
    color: (f64, f64, f64),
) {
    if series.len() < 2 {
        return;
    }
    let mut min_v = f64::INFINITY;
    let mut max_v = f64::NEG_INFINITY;
    for v in series.iter().filter_map(|p| p.1) {
        min_v = min_v.min(v);
        max_v = max_v.max(v);
    }
    if !min_v.is_finite() || !max_v.is_finite() {
        return;
    }
    if (max_v - min_v).abs() < 1e-6 {
        // flat line: center it, still visible
        min_v -= 1.0;
        max_v += 1.0;
    }
    let pad = h * 0.06;
    let span = max_v - min_v;
    let y_of = |v: f64| h - pad - (v - min_v) / span * (h - 2.0 * pad);
    let x_of = |t: f64| (t - min_t) / (max_t - min_t) * (w - 8.0) + 4.0;

    cr.set_source_rgba(color.0, color.1, color.2, 0.9);
    cr.set_line_width(1.6);
    let mut started = false;
    for &(t, v) in series {
        let Some(v) = v.filter(|v| v.is_finite()) else {
            started = false;
            continue;
        };
        if started {
            cr.line_to(x_of(t), y_of(v));
        } else {
            cr.move_to(x_of(t), y_of(v));
            started = true;
        }
    }
    let _ = cr.stroke();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trimmed_window_uses_retained_time_origin() {
        let samples = [(86_000.0, Some(1.0)), (86_100.0, None)];
        assert_eq!(time_bounds(&[&samples]), Some((86_000.0, 86_100.0)));
    }
    #[test]
    fn missing_values_break_the_stroke() {
        use libadwaita::gtk::cairo::{Format, ImageSurface};
        let mut surface = ImageSurface::create(Format::ARgb32, 100, 100).unwrap();
        let context = Context::new(&surface).unwrap();
        let samples = [
            (0.0, Some(1.0)),
            (1.0, Some(1.0)),
            (2.0, None),
            (3.0, Some(1.0)),
            (4.0, Some(1.0)),
        ];
        stroke_series(&context, &samples, (0.0, 4.0), 100.0, 100.0, TEMP);
        drop(context);
        surface.flush();
        let stride = usize::try_from(surface.stride()).unwrap_or(0);
        let data = surface.data().unwrap();
        assert_eq!(
            &data[50 * stride + 50 * 4..50 * stride + 51 * 4],
            &[0, 0, 0, 0]
        );
        assert_ne!(
            &data[50 * stride + 10 * 4..50 * stride + 11 * 4],
            &[0, 0, 0, 0]
        );
    }
}
