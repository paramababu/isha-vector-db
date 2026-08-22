//! Machine-readable output.
//!
//! Hand-written rather than derived, for the same reason the on-disk format is: this document
//! is a record that gets committed and diffed over time, and its shape should change only when
//! someone means it to — not because a serialization dependency changed its formatting.

use crate::harness::Measurement;
use crate::workloads::Scale;

/// Render results as JSON.
pub(crate) fn render(measurements: &[Measurement], scale: Scale) -> String {
    let mut out = String::from("{\n");
    out.push_str("  \"format\": 1,\n");
    out.push_str(&format!(
        "  \"engine_version\": \"{}\",\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!("  \"scale\": \"{}\",\n", escape(&scale.name())));
    out.push_str(&format!("  \"documents\": {},\n", scale.documents));
    out.push_str(&format!("  \"dimension\": {},\n", scale.dimension));
    // Recorded because a number without the machine it came from is not a measurement. A
    // comparison across different hardware is meaningless, and this is what makes that visible.
    out.push_str(&format!("  \"target\": \"{}\",\n", escape(&target())));
    out.push_str("  \"measurements\": [\n");

    for (i, m) in measurements.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!("      \"name\": \"{}\",\n", escape(&m.name)));
        out.push_str(&format!("      \"unit\": \"{}\",\n", escape(m.unit)));
        out.push_str(&format!("      \"count\": {},\n", m.count));
        out.push_str(&format!(
            "      \"total_ms\": {:.3}",
            m.total.as_secs_f64() * 1000.0
        ));
        if let Some(t) = m.throughput() {
            out.push_str(&format!(",\n      \"per_second\": {t:.1}"));
        }
        for (label, p) in [("p50", 50.0), ("p95", 95.0), ("p99", 99.0)] {
            if let Some(d) = m.percentile(p) {
                out.push_str(&format!(
                    ",\n      \"{label}_us\": {:.1}",
                    d.as_secs_f64() * 1_000_000.0
                ));
            }
        }
        if !m.notes.is_empty() {
            out.push_str(",\n      \"notes\": {");
            for (j, (k, v)) in m.notes.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(&format!("\n        \"{}\": \"{}\"", escape(k), escape(v)));
            }
            out.push_str("\n      }");
        }
        out.push_str("\n    }");
        if i + 1 < measurements.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    out
}

/// The machine these numbers came from.
///
/// Recorded because a measurement without its hardware is not a measurement, and comparing two
/// results from different machines is meaningless in a way that is easy to do by accident.
fn target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn output_is_well_formed_and_carries_the_context() {
        let mut m = Measurement::new("search_k10", "query");
        m.count = 100;
        m.total = Duration::from_millis(250);
        m.latencies = (1..=100).map(|i| Duration::from_micros(i * 10)).collect();
        m.note("dimension", 384);

        let text = render(&[m], Scale::quick());
        assert!(
            text.starts_with('{') && text.trim_end().ends_with('}'),
            "{text}"
        );
        assert!(text.contains("\"name\": \"search_k10\""), "{text}");
        assert!(text.contains("\"p99_us\""), "{text}");
        assert!(
            text.contains("\"target\""),
            "a number without its machine is not a measurement"
        );
        assert!(text.contains("\"dimension\": \"384\""), "{text}");
        // Braces balance, which is the cheapest proof that the hand-written writer is not
        // producing something a parser will reject.
        assert_eq!(
            text.chars().filter(|c| *c == '{').count(),
            text.chars().filter(|c| *c == '}').count()
        );
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(escape(r#"a "b" \ c"#), r#"a \"b\" \\ c"#);
        assert_eq!(escape("line\nbreak"), "line\\nbreak");
        assert_eq!(escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn an_empty_run_still_produces_valid_json() {
        let text = render(&[], Scale::quick());
        assert!(text.contains("\"measurements\": [\n  ]"), "{text}");
    }
}
