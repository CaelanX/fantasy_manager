//! Rust port of `fantasy_manager/web/charts.py`: server-rendered inline-SVG sparklines for the
//! /health page (no JavaScript, no chart library). Owner: work package `W9-web-views`.
//!
//! The strings are byte-identical to Python's: coordinates are rounded to 2 dp and printed with
//! `{:g}` (`fm_pycompat::fmt::general`), text is escaped like `markupsafe.escape` (`&<>'"`).
//! Python returns `Markup`; here the functions return `String` that is already HTML: W10 must
//! mark it safe (`minijinja::Value::from_safe_string`) when it hands it to a template.

use std::sync::OnceLock;

use fm_pycompat::fmt::{fixed, general};
use fm_pycompat::num::round_to;
use regex::Regex;

pub const WIDTH: i64 = 360;
pub const HEIGHT: i64 = 132;
pub const PAD_L: i64 = 40;
pub const PAD_R: i64 = 10;
pub const PAD_T: i64 = 10;
pub const PAD_B: i64 = 22;
/// baseline lines: solid, dashed, dotted, dash-dot
pub const DASHES: [&str; 4] = ["", "5 3", "1.5 2.5", "7 2 1.5 2"];

pub type Point = (f64, f64);

/// `charts.Line`.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub key: String,
    pub label: String,
    pub values: Vec<Option<f64>>,
    /// "main" | "base"
    pub role: String,
    pub dash: String,
    /// extra CSS class on the line, its points and its legend sample (per-series colours)
    pub class: Option<String>,
}

impl Line {
    pub fn new(key: &str, label: &str, values: Vec<Option<f64>>, role: &str, dash: &str) -> Line {
        Line { key: key.into(), label: label.into(), values, role: role.into(), dash: dash.into(), class: None }
    }

    pub fn with_class(mut self, class: &str) -> Line {
        self.class = Some(class.into());
        self
    }

    fn is_main(&self) -> bool {
        self.role == "main"
    }

    /// "ch-main" / "ch-base", plus the line's own class.
    fn css(&self) -> String {
        let role = if self.is_main() { "ch-main" } else { "ch-base" };
        match &self.class {
            Some(c) => format!("{role} {c}"),
            None => role.to_string(),
        }
    }
}

/// `markupsafe.escape`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|x| x.is_finite())
}

/// (lo, hi) covering every finite value with 8% headroom; a flat or empty series gets a band
/// around its value (0..1 when empty).
pub fn domain(values: &[Option<f64>], include_zero: bool) -> (f64, f64) {
    let mut vs: Vec<f64> = values.iter().filter_map(|v| finite(*v)).collect();
    if include_zero {
        vs.push(0.0);
    }
    if vs.is_empty() {
        return (0.0, 1.0);
    }
    let lo = vs.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = vs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if hi - lo < 1e-12 {
        let mut d = lo.abs() * 0.1;
        if d == 0.0 {
            d = 1.0;
        }
        return (lo - d, hi + d);
    }
    let pad = (hi - lo) * 0.08;
    (lo - pad, hi + pad)
}

/// Evenly spaced x for `n` points (one point: centred).
pub fn x_positions(n: usize, width: f64, pad_l: f64, pad_r: f64) -> Vec<f64> {
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![pad_l + (width - pad_l - pad_r) / 2.0];
    }
    let step = (width - pad_l - pad_r) / (n as f64 - 1.0);
    (0..n).map(|i| pad_l + i as f64 * step).collect()
}

/// `(PAD_L, PAD_R, PAD_T, PAD_B)` as floats.
pub const PAD: (f64, f64, f64, f64) = (PAD_L as f64, PAD_R as f64, PAD_T as f64, PAD_B as f64);

/// Data values -> SVG coordinates (y grows downwards); None for missing values.
pub fn scale_points(values: &[Option<f64>], lo: f64, hi: f64, width: f64, height: f64, pad: (f64, f64, f64, f64)) -> Vec<Option<Point>> {
    let (pl, pr, pt, pb) = pad;
    let xs = x_positions(values.len(), width, pl, pr);
    let mut span = hi - lo;
    if span == 0.0 {
        span = 1.0;
    }
    xs.iter()
        .zip(values)
        .map(|(x, v)| {
            let v = finite(*v)?;
            let y = pt + (hi - v) / span * (height - pt - pb);
            Some((round_to(*x, 2), round_to(y, 2)))
        })
        .collect()
}

fn g(x: f64) -> String {
    general(x, None, false)
}

fn fmt_point(p: Point) -> String {
    format!("{},{}", g(p.0), g(p.1))
}

/// SVG path data: a polyline through the points, broken (new `M`) at every gap.
pub fn sparkline_path(points: &[Option<Point>]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut pen_down = false;
    for p in points {
        match p {
            None => pen_down = false,
            Some(p) => {
                parts.push(format!("{}{}", if pen_down { "L" } else { "M" }, fmt_point(*p)));
                pen_down = true;
            }
        }
    }
    parts.join(" ")
}

/// Closed polygon(s) between two point series (a CI band), one per contiguous run where both
/// ends are known; a single-point run becomes a thin vertical bar.
pub fn band_path(lo_points: &[Option<Point>], hi_points: &[Option<Point>]) -> String {
    let mut runs: Vec<Vec<(Point, Point)>> = vec![vec![]];
    for (a, b) in lo_points.iter().zip(hi_points) {
        match (a, b) {
            (Some(a), Some(b)) => runs.last_mut().expect("run").push((*a, *b)),
            _ => {
                if !runs.last().expect("run").is_empty() {
                    runs.push(vec![]);
                }
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for mut run in runs {
        if run.is_empty() {
            continue;
        }
        if run.len() == 1 {
            let (a, b) = run[0];
            run = vec![((a.0 - 3.0, a.1), (b.0 - 3.0, b.1)), ((a.0 + 3.0, a.1), (b.0 + 3.0, b.1))];
        }
        let mut pts: Vec<Point> = run.iter().map(|(_, b)| *b).collect();
        pts.extend(run.iter().rev().map(|(a, _)| *a));
        let body = pts.iter().map(|p| fmt_point(*p)).collect::<Vec<_>>().join(" L");
        out.push(format!("M{body} Z"));
    }
    out.join(" ")
}

fn slug(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[^a-z0-9-]+").expect("slug regex"));
    let lower = fm_pycompat::text::py_lower(s);
    let t = re.replace_all(&lower, "-");
    let t = t.trim_matches('-');
    if t.is_empty() {
        "chart".to_string()
    } else {
        t.to_string()
    }
}

/// `'2026-10-05' -> 'Oct 5'` (anything else unchanged).
pub fn short(week: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"^(\d{4})-(\d{2})-(\d{2})$").expect("short regex"));
    let Some(m) = re.captures(week) else { return week.to_string() };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month: i64 = m[2].parse().unwrap_or(0);
    let day: i64 = m[3].parse().unwrap_or(0);
    // Python indexes MONTHS[month - 1]: month 0 wraps to "Dec", > 12 raises (unchanged here)
    let name = match month {
        0 => "Dec",
        1..=12 => MONTHS[(month - 1) as usize],
        _ => return week.to_string(),
    };
    format!("{name} {day}")
}

/// Keyword arguments of `line_chart`.
pub struct ChartOpts<'a> {
    /// (lower, upper) confidence band, one value per label
    pub band: Option<(&'a [Option<f64>], &'a [Option<f64>])>,
    pub y_fmt: &'a dyn Fn(f64) -> String,
    pub zero_line: bool,
    pub y_domain: Option<(f64, f64)>,
    pub width: i64,
    pub height: i64,
}

fn default_y_fmt(v: f64) -> String {
    fixed(v, 2, false)
}

impl Default for ChartOpts<'_> {
    fn default() -> Self {
        ChartOpts { band: None, y_fmt: &default_y_fmt, zero_line: false, y_domain: None, width: WIDTH, height: HEIGHT }
    }
}

/// One inline SVG line chart. `x` are the category labels (weeks); every line has one value per
/// label.
pub fn line_chart(chart_id: &str, x: &[String], lines: &[Line], title: &str, desc: &str, opts: &ChartOpts<'_>) -> String {
    let cid = slug(chart_id);
    let (width, height) = (opts.width, opts.height);
    let y_fmt = opts.y_fmt;
    let mut allv: Vec<Option<f64>> = lines.iter().flat_map(|l| l.values.iter().copied()).collect();
    if let Some((lo, hi)) = opts.band {
        allv.extend(lo.iter().copied());
        allv.extend(hi.iter().copied());
    }
    let (lo, hi) = opts.y_domain.unwrap_or_else(|| domain(&allv, opts.zero_line));
    let sp = |vals: &[Option<f64>]| scale_points(vals, lo, hi, width as f64, height as f64, PAD);
    let bottom = height - PAD_B;
    let top = PAD_T;
    let e = escape;
    let mut svg: Vec<String> = vec![
        format!(
            "<svg class=\"spark\" id=\"{cid}\" viewBox=\"0 0 {width} {height}\" role=\"img\" aria-labelledby=\"{cid}-t {cid}-d\" xmlns=\"http://www.w3.org/2000/svg\" preserveAspectRatio=\"xMidYMid meet\">"
        ),
        format!("<title id=\"{cid}-t\">{}</title><desc id=\"{cid}-d\">{}</desc>", e(title), e(desc)),
        "<g class=\"ch-axis\" aria-hidden=\"true\">".to_string(),
        format!("<line x1=\"{PAD_L}\" x2=\"{}\" y1=\"{top}\" y2=\"{top}\" class=\"ch-grid\"/>", width - PAD_R),
        format!("<line x1=\"{PAD_L}\" x2=\"{}\" y1=\"{bottom}\" y2=\"{bottom}\" class=\"ch-grid\"/>", width - PAD_R),
        format!("<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{}</text>", PAD_L - 5, top + 4, e(&y_fmt(hi))),
        format!("<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{}</text>", PAD_L - 5, bottom + 1, e(&y_fmt(lo))),
    ];
    if opts.zero_line && lo < 0.0 && 0.0 < hi {
        if let Some((_, y0)) = sp(&[Some(0.0)]).into_iter().flatten().next() {
            svg.push(format!(
                "<line x1=\"{PAD_L}\" x2=\"{}\" y1=\"{}\" y2=\"{}\" class=\"ch-zero\"/>",
                width - PAD_R,
                g(y0),
                g(y0)
            ));
        }
    }
    if !x.is_empty() {
        let xs = x_positions(x.len(), width as f64, PAD.0, PAD.1);
        svg.push(format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"{}\">{}</text>",
            g(xs[0]),
            height - 5,
            if x.len() == 1 { "middle" } else { "start" },
            e(&short(&x[0]))
        ));
        if x.len() > 1 {
            svg.push(format!(
                "<text x=\"{}\" y=\"{}\" text-anchor=\"end\">{}</text>",
                g(xs[xs.len() - 1]),
                height - 5,
                e(&short(&x[x.len() - 1]))
            ));
        }
    }
    svg.push("</g>".to_string());
    if let Some((blo, bhi)) = opts.band {
        let d = band_path(&sp(blo), &sp(bhi));
        if !d.is_empty() {
            svg.push(format!("<path class=\"ch-band\" d=\"{d}\" aria-hidden=\"true\"/>"));
        }
    }
    // main line drawn last (on top); stable
    let mut order: Vec<&Line> = lines.iter().collect();
    order.sort_by_key(|l| l.is_main());
    for ln in order {
        let pts = sp(&ln.values);
        let d = sparkline_path(&pts);
        if d.is_empty() {
            continue;
        }
        let cls = ln.css();
        let dash = if ln.dash.is_empty() { String::new() } else { format!(" stroke-dasharray=\"{}\"", ln.dash) };
        svg.push(format!("<path class=\"{cls}\" d=\"{d}\"{dash} data-key=\"{}\" aria-hidden=\"true\"/>", e(&ln.key)));
        if ln.is_main() {
            let pt_cls = match &ln.class {
                Some(c) => format!("ch-pt {c}"),
                None => "ch-pt".to_string(),
            };
            for (i, p) in pts.iter().enumerate() {
                let Some(p) = p else { continue };
                let xl = x.get(i).cloned().unwrap_or_else(|| i.to_string());
                let mut lab = format!("{xl}: {} {}", ln.label, y_fmt(ln.values[i].unwrap_or(f64::NAN)));
                if let Some((blo, bhi)) = opts.band {
                    if let (Some(a), Some(b)) = (blo.get(i).and_then(|v| finite(*v)), bhi.get(i).and_then(|v| finite(*v))) {
                        lab.push_str(&format!(" (95% CI {} to {})", y_fmt(a), y_fmt(b)));
                    }
                }
                svg.push(format!(
                    "<g class=\"{pt_cls}\"><circle class=\"ch-hit\" cx=\"{cx}\" cy=\"{cy}\" r=\"9\"/><circle class=\"ch-dot\" cx=\"{cx}\" cy=\"{cy}\" r=\"3.5\"/><title>{}</title></g>",
                    e(&lab),
                    cx = g(p.0),
                    cy = g(p.1)
                ));
            }
        }
    }
    svg.push("</svg>".to_string());
    svg.concat()
}

/// A small HTML legend (line samples drawn as SVG so dashes match the chart).
pub fn legend(lines: &[Line], band_label: Option<&str>) -> String {
    let mut order: Vec<&Line> = lines.iter().collect();
    order.sort_by_key(|l| !l.is_main());
    let mut items: Vec<String> = Vec::new();
    for ln in order {
        let cls = ln.css();
        let dash = if ln.dash.is_empty() { String::new() } else { format!(" stroke-dasharray=\"{}\"", ln.dash) };
        items.push(format!(
            "<li><svg viewBox=\"0 0 22 8\" width=\"22\" height=\"8\" aria-hidden=\"true\"><line x1=\"1\" x2=\"21\" y1=\"4\" y2=\"4\" class=\"{cls}\"{dash}/></svg>{}</li>",
            escape(&ln.label)
        ));
    }
    if let Some(label) = band_label.filter(|l| !l.is_empty()) {
        items.push(format!(
            "<li><svg viewBox=\"0 0 22 8\" width=\"22\" height=\"8\" aria-hidden=\"true\"><rect x=\"1\" y=\"0\" width=\"20\" height=\"8\" class=\"ch-band\"/></svg>{}</li>",
            escape(label)
        ));
    }
    format!("<ul class=\"ch-legend\">{}</ul>", items.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_cases() {
        assert_eq!(domain(&[], false), (0.0, 1.0));
        assert_eq!(domain(&[Some(2.0), Some(2.0)], false), (1.8, 2.2));
        assert_eq!(domain(&[Some(0.0), None], false), (-1.0, 1.0));
        let (lo, hi) = domain(&[Some(1.0), Some(3.0)], false);
        assert!((lo - 0.84).abs() < 1e-12 && (hi - 3.16).abs() < 1e-12);
    }

    #[test]
    fn short_dates() {
        assert_eq!(short("2026-10-05"), "Oct 5");
        assert_eq!(short("wk"), "wk");
    }

    #[test]
    fn escaping_and_slug() {
        assert_eq!(escape("a<b>&'\""), "a&lt;b&gt;&amp;&#39;&#34;");
        assert_eq!(slug("My Chart #1"), "my-chart-1");
        assert_eq!(slug(""), "chart");
    }

    #[test]
    fn line_class_is_optional() {
        let x = vec!["a".to_string(), "b".to_string()];
        let opts = ChartOpts::default();
        let plain = Line::new("k", "K", vec![Some(1.0), Some(2.0)], "main", "");
        let svg = line_chart("c", &x, &[plain.clone()], "t", "d", &opts);
        assert!(svg.contains("<path class=\"ch-main\" d=") && svg.contains("<g class=\"ch-pt\">"));
        let coloured = plain.with_class("ch-s2");
        let svg = line_chart("c", &x, &[coloured.clone()], "t", "d", &opts);
        assert!(svg.contains("<path class=\"ch-main ch-s2\" d=") && svg.contains("<g class=\"ch-pt ch-s2\">"));
        assert!(legend(&[coloured], None).contains("class=\"ch-main ch-s2\""));
    }

    #[test]
    fn paths_break_at_gaps() {
        let pts = [Some((1.0, 2.0)), None, Some((3.5, 4.25)), Some((5.0, 6.0))];
        assert_eq!(sparkline_path(&pts), "M1,2 M3.5,4.25 L5,6");
    }
}
