//! Turning numbers into the short phrases the windows show. Kept apart from
//! the UI wiring so it can be tested without a window.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use hatchzip_core::human;
use hatchzip_core::session::SpacePlan;

use crate::PlanView;

/// What a long open says while it reads through the archive. Rounded down,
/// so it never claims 100% before the read is done.
pub fn reading_text(fraction: f64) -> String {
    format!("Reading archive… {}%", (fraction * 100.0).floor() as u32)
}

/// `3412` as `3,412`.
pub fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Why a restore left replaced files where they are, and where that is.
pub fn set_aside(n: usize, output: &Path) -> String {
    let (files, them) = if n == 1 {
        (
            "1 replaced file remains".to_string(),
            "the file extracted at its path is",
        )
    } else {
        (
            format!("{} replaced files remain", count(n)),
            "the files extracted at their paths are",
        )
    };
    format!(
        "{files} in {}, since {them} no longer in the archive.",
        hatchzip_core::replace::folder(output).display()
    )
}

/// A time remaining, rounded to the unit that matters.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let unit = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match s {
        0..=4 => "less than 5 seconds".to_string(),
        5..=59 => unit(s, "second", "seconds"),
        60..=3599 => unit(s.div_ceil(60), "minute", "minutes"),
        _ => format!(
            "{} {}",
            unit(s / 3600, "hour", "hours"),
            unit((s % 3600) / 60, "minute", "minutes")
        ),
    }
}

/// The "keep free" box takes gigabytes, like `2` or `0.5`.
pub fn parse_gb(text: &str) -> Option<u64> {
    let text = text.trim().replace(',', ".");
    if text.is_empty() {
        return Some(0);
    }
    let gb: f64 = text.parse().ok()?;
    // Under a million gigabytes: more is a typo, and would overflow.
    (0.0..1e6).contains(&gb).then_some((gb * 1e9) as u64)
}

pub fn gb_text(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb.fract() == 0.0 {
        format!("{gb:.0}")
    } else {
        format!("{gb}")
    }
}

pub fn plan_view(plan: &SpacePlan) -> PlanView {
    let verdict = if plan.fits {
        "Sufficient disk space".to_string()
    } else {
        let short = plan.needed + plan.keep_free - plan.free;
        format!(
            "Insufficient disk space. {} more is required.",
            human(short)
        )
    };
    let mut notes = Vec::new();
    // Whichever way was not chosen, when it would change the answer or
    // offers more. Only a fresh archive can still choose.
    if plan.either && plan.keep_archive && !plan.fits && plan.in_place_fits() {
        notes.push(format!(
            "Removing the archive during extraction requires only {}.",
            human(plan.in_place_needs)
        ));
    }
    if plan.either && !plan.keep_archive && plan.fits && plan.normal_fits() {
        notes.push("There is also enough space to keep the archive.".to_string());
    }
    if !plan.fits && plan.solid_block_bound {
        notes.push(
            "Most of the required space is one solid block, which can only be released after \
             it is fully extracted."
                .to_string(),
        );
    }
    if !plan.reclaims && !plan.keep_archive {
        notes.push(
            "The destination is on another drive, so removing the archive does not free space there."
                .to_string(),
        );
    }
    if plan.archive_kept_whole {
        notes.push(
            "This drive cannot release space inside a file, so the compressed archive remains \
             whole until extraction completes."
                .to_string(),
        );
    }
    let used = plan.needed + plan.keep_free;
    PlanView {
        ready: true,
        free: human(plan.free).into(),
        // A half-extracted run continues the way it started.
        normal: if !plan.keep_archive && !plan.either {
            "-".into()
        } else {
            human(plan.normal_needs).into()
        },
        in_place: if plan.keep_archive && !plan.either {
            "-".into()
        } else {
            human(plan.in_place_needs).into()
        },
        keep_archive: plan.keep_archive,
        needed: human(plan.needed).into(),
        keep: human(plan.keep_free).into(),
        after: if plan.fits {
            human(plan.free_after).into()
        } else {
            "-".into()
        },
        fits: plan.fits,
        verdict: verdict.into(),
        note: notes.join(" ").into(),
        meter: if plan.free == 0 {
            1.0
        } else {
            (used as f64 / plan.free as f64).min(1.0) as f32
        },
    }
}

/// Speed over the last few seconds, so one slow batch flush doesn't make
/// the estimate jump around.
pub struct Rate {
    samples: VecDeque<(Instant, u64)>,
    window: Duration,
}

impl Default for Rate {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            window: Duration::from_secs(5),
        }
    }
}

impl Rate {
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    pub fn add(&mut self, at: Instant, done: u64) {
        while let Some(&(t, _)) = self.samples.front() {
            if at.duration_since(t) > self.window && self.samples.len() > 1 {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        self.samples.push_back((at, done));
    }

    /// Bytes per second, once there is a second of history.
    pub fn per_second(&self) -> Option<f64> {
        let (&(t0, b0), &(t1, b1)) = (self.samples.front()?, self.samples.back()?);
        let secs = t1.duration_since(t0).as_secs_f64();
        (secs >= 1.0).then(|| b1.saturating_sub(b0) as f64 / secs)
    }

    /// "120.5 MB/s, 3 minutes remaining"
    pub fn describe(&self, left: u64) -> String {
        match self.per_second() {
            Some(r) if r >= 1.0 => format!(
                "{}/s, {} remaining",
                human(r as u64),
                duration(Duration::from_secs_f64(left as f64 / r))
            ),
            Some(_) => "Waiting for disk".to_string(),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_group_thousands() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(3_412_001), "3,412,001");
    }

    #[test]
    fn durations_round_like_people_do() {
        assert_eq!(duration(Duration::from_secs(2)), "less than 5 seconds");
        assert_eq!(duration(Duration::from_secs(42)), "42 seconds");
        assert_eq!(duration(Duration::from_secs(60)), "1 minute");
        assert_eq!(duration(Duration::from_secs(61)), "2 minutes");
        assert_eq!(
            duration(Duration::from_secs(3 * 3600 + 125)),
            "3 hours 2 minutes"
        );
        assert_eq!(duration(Duration::from_secs(3600 + 60)), "1 hour 1 minute");
    }

    #[test]
    fn keep_free_takes_gigabytes() {
        assert_eq!(parse_gb("2"), Some(2_000_000_000));
        assert_eq!(parse_gb(" 0.5 "), Some(500_000_000));
        assert_eq!(parse_gb("1,5"), Some(1_500_000_000));
        assert_eq!(parse_gb(""), Some(0));
        assert_eq!(parse_gb("-1"), None);
        assert_eq!(parse_gb("two"), None);
        assert_eq!(gb_text(2_000_000_000), "2");
        assert_eq!(gb_text(500_000_000), "0.5");
    }

    #[test]
    fn rate_needs_a_second_of_history() {
        let mut rate = Rate::default();
        let t = Instant::now();
        rate.add(t, 0);
        rate.add(t + Duration::from_millis(500), 50_000_000);
        assert_eq!(rate.per_second(), None);
        assert_eq!(rate.describe(1), "");
        rate.add(t + Duration::from_secs(2), 200_000_000);
        assert_eq!(rate.per_second(), Some(100_000_000.0));
        assert_eq!(
            rate.describe(6_000_000_000),
            "100.0 MB/s, 1 minute remaining"
        );
    }

    #[test]
    fn rate_forgets_old_samples() {
        let mut rate = Rate::default();
        let t = Instant::now();
        rate.add(t, 0);
        // A burst long ago, then a steady 10 MB/s.
        rate.add(t + Duration::from_secs(1), 1_000_000_000);
        for i in 2..=20u64 {
            rate.add(
                t + Duration::from_secs(i),
                1_000_000_000 + (i - 1) * 10_000_000,
            );
        }
        let r = rate.per_second().unwrap();
        assert!((r - 10_000_000.0).abs() < 1.0, "{r}");
    }

    fn plan(free: u64, needed: u64, keep: u64) -> SpacePlan {
        SpacePlan {
            free,
            needed,
            keep_free: keep,
            fits: free >= needed + keep,
            free_after: free.saturating_sub(needed),
            keep_archive: false,
            normal_needs: needed * 2,
            in_place_needs: needed,
            either: true,
            reclaims: true,
            solid_block_bound: false,
            archive_kept_whole: false,
        }
    }

    /// The same disk and archive, planned with the archive kept.
    fn kept(mut p: SpacePlan) -> SpacePlan {
        p.keep_archive = true;
        p.needed = p.normal_needs;
        p.fits = p.free >= p.needed + p.keep_free;
        p
    }

    #[test]
    fn the_other_way_is_mentioned_when_it_matters() {
        // Room for 4 GB: in place needs 3 GB, keeping needs 6 GB.
        let tight = plan(6_000_000_000, 3_000_000_000, 2_000_000_000);
        let v = plan_view(&kept(tight));
        assert!(!v.fits && v.keep_archive);
        assert_eq!(
            v.verdict,
            "Insufficient disk space. 2.0 GB more is required."
        );
        assert_eq!(v.needed, "6.0 GB");
        assert_eq!(v.normal, "6.0 GB");
        assert_eq!(v.in_place, "3.0 GB");
        assert_eq!(
            v.note,
            "Removing the archive during extraction requires only 3.0 GB."
        );
        // In place fits and says nothing about keeping, which doesn't.
        let v = plan_view(&plan(6_000_000_000, 3_000_000_000, 2_000_000_000));
        assert!(v.fits && !v.keep_archive);
        assert_eq!(v.note, "");

        let roomy = plan(20_000_000_000, 3_000_000_000, 2_000_000_000);
        assert_eq!(
            plan_view(&roomy).note,
            "There is also enough space to keep the archive."
        );
        assert_eq!(plan_view(&kept(roomy)).note, "");

        // Half extracted, a run goes on the way it started.
        let mut resumed = kept(plan(1_000_000_000, 3_000_000_000, 0));
        resumed.either = false;
        let v = plan_view(&resumed);
        assert_eq!(v.in_place, "-");
        assert_eq!(v.note, "");
        let mut resumed = plan(1_000_000_000, 300_000_000, 0);
        resumed.either = false;
        assert_eq!(plan_view(&resumed).normal, "-");
    }

    #[test]
    fn plans_read_plainly() {
        let fits = plan_view(&plan(10_000_000_000, 3_000_000_000, 2_000_000_000));
        assert!(fits.fits);
        assert_eq!(fits.verdict, "Sufficient disk space");
        assert_eq!(fits.needed, "3.0 GB");
        assert_eq!(fits.in_place, "3.0 GB");
        assert_eq!(fits.normal, "6.0 GB");
        assert_eq!(fits.note, "There is also enough space to keep the archive.");
        assert!((fits.meter - 0.5).abs() < 1e-6);

        let mut tight = plan(4_000_000_000, 3_000_000_000, 2_000_000_000);
        tight.solid_block_bound = true;
        tight.reclaims = false;
        let v = plan_view(&tight);
        assert!(!v.fits);
        assert_eq!(
            v.verdict,
            "Insufficient disk space. 1.0 GB more is required."
        );
        assert_eq!(v.after, "-");
        assert!(v.note.contains("solid block"));
        assert!(v.note.contains("another drive"));
        assert_eq!(v.meter, 1.0);
    }
}
