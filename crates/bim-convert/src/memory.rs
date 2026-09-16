//! What this machine can afford to convert.
//!
//! A whole-file reader holds the source, the table it parses into and the
//! model it builds from that table, all at once, so the question "is this file
//! too large" is really "is this file too large *here*". That answer belongs
//! beside [`Format::memory_ratio`](crate::Format::memory_ratio) rather than in
//! any one caller: the server asks it before accepting an upload, and the
//! converter asks it again before paying for a read, and the two must not
//! disagree - a file the server accepts and the converter then refuses is the
//! worst of both answers.

use crate::Format;

/// The share of the memory budget one conversion may plan to occupy.
///
/// A conversion is not the only thing on the machine. Sizing the ceiling to
/// the *whole* budget is what turned a workstation with an editor and a
/// browser open into a swapping brick: the arithmetic said the file fit, and
/// it did, with nothing left for anything else. Half is what a single job may
/// assume, and [`room_to_convert`] still checks the moment it starts.
const CONVERSION_SHARE: u64 = 2;

/// Smallest IFC ceiling, whatever the host says. Below this the reader is
/// being denied files it has always managed.
pub const MIN_MAX_IFC_BYTES: u64 = 256 * 1024 * 1024;

/// Largest IFC ceiling this will choose on its own.
///
/// A ceiling a host cannot meet is not raised by naming a bigger number here:
/// what actually decides is [`memory_budget`] divided by the ratio and the
/// share, and this only stops a very large machine from advertising a figure
/// nobody has ever run. Eight gigabytes of source is about 37 GB of memory at
/// the measured ratio, which is a 64 GB machine converting one file and
/// nothing else - the point past which an operator should be saying so
/// deliberately rather than discovering it.
pub const MAX_AUTOMATIC_IFC_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// The memory this process may actually use, in bytes.
///
/// Inside a container `/proc/meminfo` reports the *host's* memory, not the
/// cgroup's limit, so a 4 GB container reads 59 GB and plans to use all of it
/// until the kernel kills it. The cgroup limit is checked first for that
/// reason, v2 then v1, and `MemTotal` is the fallback for a bare host.
#[must_use]
pub fn memory_budget() -> Option<u64> {
    let cgroup = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .into_iter()
    .filter_map(|path| std::fs::read_to_string(path).ok())
    .find_map(|text| text.trim().parse::<u64>().ok())
    // An unlimited cgroup reports "max" (v2) or a number near u64::MAX
    // (v1), neither of which is a budget.
    .filter(|limit| *limit < u64::MAX / 2);
    cgroup
        .or_else(|| meminfo_field("MemTotal:"))
        .filter(|budget| *budget > 0)
}

/// Memory free *now*, as the kernel's own estimate of what can be handed out
/// without swapping.
#[must_use]
pub fn available_memory() -> Option<u64> {
    meminfo_field("MemAvailable:")
}

/// One `/proc/meminfo` field, in bytes.
fn meminfo_field(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = status.lines().find(|line| line.starts_with(field))?;
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|kilobytes| kilobytes.checked_mul(1024))
}

/// Largest IFC this host will be asked to convert.
///
/// The reader holds the whole entity table in memory, so the ceiling belongs
/// to the machine rather than to a constant: 256 MB refused files a
/// workstation converts in twelve seconds, and the same number is still too
/// generous on a small container. This scales between the two and stops at
/// [`MAX_AUTOMATIC_IFC_BYTES`], because a default should be safe on a busy
/// machine rather than merely arithmetically possible on an idle one.
#[must_use]
pub fn max_ifc_bytes() -> u64 {
    memory_budget()
        .map_or(MIN_MAX_IFC_BYTES, |budget| {
            budget / (Format::Ifc.memory_ratio().unwrap_or(1) * CONVERSION_SHARE)
        })
        .clamp(MIN_MAX_IFC_BYTES, MAX_AUTOMATIC_IFC_BYTES)
}

/// The memory reading a source of `bytes` in this format is expected to cost,
/// or `None` where the format's cost does not scale with its source.
#[must_use]
pub fn memory_to_convert(bytes: u64, format: Format) -> Option<u64> {
    Some(bytes.saturating_mul(format.memory_ratio()?))
}

/// Whether there is memory free *now* to convert a source of `bytes`, or the
/// message explaining why not.
///
/// The ceiling above is a plan made from capacity; this is the check against
/// the moment. Refusing here costs the caller a clear error, where going ahead
/// costs everyone the machine - and a host that swaps is not one anybody can
/// see a progress bar on.
///
/// # Errors
///
/// The message naming what the conversion needs and what is free.
pub fn room_to_convert(bytes: u64, format: Format) -> Result<(), String> {
    // Only a whole-file reader's cost scales with the source, and only such a
    // format declares a ratio. An RVT is bounded a member at a time instead.
    let Some(ratio) = format.memory_ratio() else {
        return Ok(());
    };
    let Some(needed) = bytes.checked_mul(ratio) else {
        return Err("this file is too large to convert".to_owned());
    };
    let Some(free) = available_memory() else {
        return Ok(());
    };
    if needed > free {
        return Err(format!(
            "converting this {} MB {} needs about {} MB of memory and only {} MB is free; \
             close something or try again",
            megabytes(bytes),
            format.label(),
            megabytes(needed),
            megabytes(free)
        ));
    }
    Ok(())
}

/// Whether there is memory free now to convert a whole federation.
///
/// The sources are read one after another but their models are held together,
/// so what has to fit is the sum. Only a whole-file reader's cost scales with
/// its source, so only those contribute.
///
/// # Errors
///
/// The message explaining which conversion will not fit.
pub fn room_to_convert_all(sources: &[(u64, Format)]) -> Result<(), String> {
    let mut planned = 0_u64;
    for (bytes, format) in sources {
        if format.memory_ratio().is_some() {
            planned = planned.saturating_add(*bytes);
        }
    }
    // Charged against the format that actually scales; a set of RVTs plans
    // nothing here and is bounded a member at a time, exactly as one is.
    room_to_convert(planned, Format::Ifc)
}

#[must_use]
pub fn megabytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A federation's models are held together, so what has to fit is the
    /// sum - and only the formats whose cost scales with their source count.
    #[test]
    fn a_federation_is_charged_as_the_sum_of_its_whole_file_readers() {
        let ceiling = 8 * MIN_MAX_IFC_BYTES;
        // Well inside anything, whether counted once or three times.
        let small = [(1 << 20, Format::Ifc); 3];
        assert!(room_to_convert_all(&small).is_ok());
        // An RVT is bounded a member at a time, so a set of them plans
        // nothing here however large they are.
        let huge_rvt = [(ceiling, Format::Rvt); 4];
        assert!(room_to_convert_all(&huge_rvt).is_ok());
        // And the sum is what is charged, not the largest: three sources that
        // each fit can still fail together, if the host is small enough to say
        // so. Only assert the arithmetic, since free memory is not ours.
        let summed = [(1 << 30, Format::Ifc), (1 << 30, Format::Ifc)];
        let one = [(2 << 30, Format::Ifc)];
        assert_eq!(
            room_to_convert_all(&summed).is_ok(),
            room_to_convert_all(&one).is_ok()
        );
    }

    #[test]
    fn the_ifc_ceiling_stays_between_its_floor_and_what_may_be_chosen_unasked() {
        let ceiling = max_ifc_bytes();
        assert!(ceiling >= MIN_MAX_IFC_BYTES, "{ceiling} is below the floor");
        // The point of the clamp: a big machine must not talk this into a
        // ceiling nobody has ever run just because the arithmetic allows it.
        assert!(
            ceiling <= MAX_AUTOMATIC_IFC_BYTES,
            "{ceiling} is above what may be chosen without being asked"
        );
        // Reading twice gives the same answer - the ceiling is capacity, not
        // whatever is free this second.
        assert_eq!(max_ifc_bytes(), ceiling);
        if std::path::Path::new("/proc/meminfo").exists() {
            assert!(memory_budget().is_some_and(|budget| budget > 0));
        }
    }

    #[test]
    fn refuses_a_conversion_the_free_memory_will_not_hold() {
        // An RVT is not held in memory this way and is never refused here.
        assert!(room_to_convert(u64::MAX, Format::Rvt).is_ok());
        // A small IFC always fits.
        assert!(room_to_convert(1024, Format::Ifc).is_ok());
        // One the size of the machine does not, and says so rather than
        // taking the host down to find out.
        let refusal = room_to_convert(u64::MAX / 2, Format::Ifc);
        assert!(refusal.is_err(), "an impossible conversion was allowed");
    }

    /// Nothing may overflow into a permission: a size no arithmetic can
    /// multiply is refused rather than wrapping into a small number.
    #[test]
    fn what_no_host_can_hold_is_refused_rather_than_overflowing() {
        let refusal = room_to_convert(u64::MAX, Format::Ifc).expect_err("a refusal");
        assert!(refusal.contains("too large"), "{refusal}");
    }

    #[test]
    fn only_a_whole_file_reader_plans_memory_from_its_source() {
        assert_eq!(memory_to_convert(1 << 30, Format::Rvt), None);
        assert_eq!(
            memory_to_convert(1 << 30, Format::Ifc),
            Some((1 << 30) * Format::Ifc.memory_ratio().expect("a ratio"))
        );
    }
}

