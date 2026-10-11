//! The `stats` module: descriptive statistics over an iterable of
//! numbers. Every function accepts any Rune iterable (vector, object
//! values, range, lazy iterator chain) whose items are integers or
//! floats, mixed freely, and computes in `f64`. A non-number item is a
//! VM error. The iterable is read, not taken, so the caller's value
//! stays usable.

use std::fmt;

use rune::runtime::{Iterator, Value, VmError};
use rune::{ContextError, Module, TypeHash};

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("stats")?;
    m.function("count", count).build()?;
    m.function("sum", sum).build()?;
    m.function("min", min).build()?;
    m.function("max", max).build()?;
    m.function("mean", mean).build()?;
    m.function("median", median).build()?;
    m.function("variance", variance).build()?;
    m.function("stddev", stddev).build()?;
    m.function("sample_variance", sample_variance).build()?;
    m.function("sample_stddev", sample_stddev).build()?;
    m.function("percentile", percentile).build()?;
    Ok(m)
}

/// The number of items.
fn count(values: Iterator) -> Result<u64, VmError> {
    Ok(collect(values)?.len() as u64)
}

/// The sum of the items; `0.0` when there are none.
fn sum(values: Iterator) -> Result<f64, VmError> {
    Ok(collect(values)?.iter().sum())
}

/// The smallest item, or `None` when there are none.
fn min(values: Iterator) -> Result<Option<f64>, VmError> {
    Ok(collect(values)?.into_iter().reduce(f64::min))
}

/// The largest item, or `None` when there are none.
fn max(values: Iterator) -> Result<Option<f64>, VmError> {
    Ok(collect(values)?.into_iter().reduce(f64::max))
}

/// The arithmetic mean, or `None` when there are no items.
fn mean(values: Iterator) -> Result<Option<f64>, VmError> {
    Ok(mean_of(&collect(values)?))
}

/// The median, the 50th percentile, or `None` when there are no items.
fn median(values: Iterator) -> Result<Option<f64>, VmError> {
    let mut xs = collect(values)?;
    Ok(percentile_of(&mut xs, 50.0))
}

/// The population variance, the mean squared deviation from the mean,
/// or `None` when there are no items.
fn variance(values: Iterator) -> Result<Option<f64>, VmError> {
    let xs = collect(values)?;
    Ok(squared_deviation(&xs).map(|ss| ss / xs.len() as f64))
}

/// The population standard deviation, or `None` when there are no
/// items.
fn stddev(values: Iterator) -> Result<Option<f64>, VmError> {
    Ok(variance(values)?.map(f64::sqrt))
}

/// The sample variance, with Bessel's correction (`n - 1`), or `None`
/// when there are fewer than two items.
fn sample_variance(values: Iterator) -> Result<Option<f64>, VmError> {
    let xs = collect(values)?;
    if xs.len() < 2 {
        return Ok(None);
    }
    Ok(squared_deviation(&xs).map(|ss| ss / (xs.len() - 1) as f64))
}

/// The sample standard deviation, or `None` when there are fewer than
/// two items.
fn sample_stddev(values: Iterator) -> Result<Option<f64>, VmError> {
    Ok(sample_variance(values)?.map(f64::sqrt))
}

/// The `p`th percentile for `p` in `0..=100`, interpolating linearly
/// between the two nearest ranks, or `None` when there are no items.
/// A `p` outside the range is a VM error.
fn percentile(values: Iterator, p: f64) -> Result<Option<f64>, VmError> {
    if !(0.0..=100.0).contains(&p) {
        return Err(VmError::panic(format!(
            "percentile: expected p in 0..=100, got {p}"
        )));
    }
    let mut xs = collect(values)?;
    Ok(percentile_of(&mut xs, p))
}

fn mean_of(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| xs.iter().sum::<f64>() / xs.len() as f64)
}

/// The sum of squared deviations from the mean, or `None` when there
/// are no items.
fn squared_deviation(xs: &[f64]) -> Option<f64> {
    let mean = mean_of(xs)?;
    Some(xs.iter().map(|x| (x - mean).powi(2)).sum())
}

/// The percentile of `xs`, which is sorted in place. The rank is
/// `p / 100 * (n - 1)`; a fractional rank interpolates between the
/// items on either side.
fn percentile_of(xs: &mut [f64], p: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(f64::total_cmp);
    let rank = p / 100.0 * (xs.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let frac = rank - lo as f64;
    // `rank` is at most `len - 1`, so both ranks are in range
    let below = *xs.get(lo)?;
    let above = *xs.get(rank.ceil() as usize)?;
    Some(below + (above - below) * frac)
}

/// The items of an iterable as `f64`.
fn collect(mut values: Iterator) -> Result<Vec<f64>, VmError> {
    let mut out = Vec::new();
    while let Some(value) = values.next()? {
        out.push(to_f64(&value)?);
    }
    Ok(out)
}

/// Coerce a Rune value to `f64`, accepting integers as well as floats.
fn to_f64(value: &Value) -> Result<f64, VmError> {
    let hash = value.type_hash();

    if hash == f64::HASH {
        Ok(value.as_float().unwrap())
    } else if hash == i64::HASH {
        Ok(value.as_signed().unwrap() as f64)
    } else if hash == u64::HASH {
        Ok(value.as_unsigned().unwrap() as f64)
    } else {
        Err(VmError::panic(ExpectedNumber {
            actual: value.type_info().to_string(),
        }))
    }
}

/// Raised when a value fed to a numeric aggregation is not a number.
///
/// Rune's own `ExpectedNumber` error kind is unreachable (its `VmErrorKind` is
/// crate-private), so we carry our own and surface it through `VmError::panic`.
#[derive(Debug)]
struct ExpectedNumber {
    actual: String,
}

impl fmt::Display for ExpectedNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected a number, but found `{}`", self.actual)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rune::sync::Arc;
    use rune::{Context, Vm};

    fn call(expr: &str) -> Result<Value, VmError> {
        let mut context = Context::with_default_modules().unwrap();
        context.install(module().unwrap()).unwrap();
        let runtime = Arc::try_new(context.runtime().unwrap()).unwrap();

        let mut sources = rune::Sources::new();
        sources
            .insert(rune::Source::memory(format!("pub fn main() {{ {expr} }}")).unwrap())
            .unwrap();

        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .build()
            .unwrap();
        let mut vm = Vm::new(runtime, Arc::try_new(unit).unwrap());

        vm.call(["main"], ())
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    fn opt_of(expr: &str) -> Option<f64> {
        rune::from_value::<Option<f64>>(call(expr).unwrap()).unwrap()
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    fn f64_of(expr: &str) -> f64 {
        rune::from_value::<f64>(call(expr).unwrap()).unwrap()
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "takes the VM execution's return value; the test holds the only live handle"
    )]
    fn u64_of(expr: &str) -> u64 {
        rune::from_value::<u64>(call(expr).unwrap()).unwrap()
    }

    fn approx(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn mean_of_ints() {
        assert_eq!(opt_of("stats::mean([1, 2, 3])"), Some(2.0));
    }

    #[test]
    fn mean_of_floats() {
        assert_eq!(opt_of("stats::mean([1.0, 2.0, 3.0])"), Some(2.0));
    }

    #[test]
    fn mean_mixes_ints_and_floats() {
        assert_eq!(opt_of("stats::mean([1, 2.5, 4])"), Some(2.5));
    }

    #[test]
    fn mean_of_empty_is_none() {
        assert_eq!(opt_of("stats::mean([])"), None);
    }

    #[test]
    fn mean_consumes_object_values() {
        assert_eq!(opt_of("stats::mean(#{ a: 2, b: 4 }.values())"), Some(3.0));
    }

    #[test]
    fn mean_consumes_lazy_iterator() {
        assert_eq!(
            opt_of("stats::mean([1, 2, 3, 4].iter().filter(|n| n % 2 == 0))"),
            Some(3.0),
        );
    }

    #[test]
    fn mean_rejects_non_numbers() {
        let error = call("stats::mean([1, \"two\", 3])").unwrap_err();
        assert!(
            error.to_string().contains("expected a number"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn count_and_sum_of_empty_are_zero() {
        assert_eq!(u64_of("stats::count([])"), 0);
        assert_eq!(f64_of("stats::sum([])"), 0.0);
    }

    #[test]
    fn count_and_sum() {
        assert_eq!(u64_of("stats::count([3, 1, 2])"), 3);
        assert_eq!(f64_of("stats::sum([3, 1, 2.5])"), 6.5);
    }

    #[test]
    fn min_and_max() {
        assert_eq!(opt_of("stats::min([3, 1, 2])"), Some(1.0));
        assert_eq!(opt_of("stats::max([3, 1, 2])"), Some(3.0));
        assert_eq!(opt_of("stats::min([])"), None);
        assert_eq!(opt_of("stats::max([])"), None);
    }

    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(opt_of("stats::median([3, 1, 2])"), Some(2.0));
        assert_eq!(opt_of("stats::median([4, 1, 3, 2])"), Some(2.5));
        assert_eq!(opt_of("stats::median([])"), None);
    }

    #[test]
    fn variance_and_stddev_are_population_measures() {
        assert_eq!(
            opt_of("stats::variance([2, 4, 4, 4, 5, 5, 7, 9])"),
            Some(4.0)
        );
        assert_eq!(opt_of("stats::stddev([2, 4, 4, 4, 5, 5, 7, 9])"), Some(2.0));
        assert_eq!(opt_of("stats::variance([7])"), Some(0.0));
        assert_eq!(opt_of("stats::variance([])"), None);
    }

    #[test]
    fn sample_variance_and_stddev_use_bessel_correction() {
        assert!(approx(
            opt_of("stats::sample_variance([2, 4, 4, 4, 5, 5, 7, 9])"),
            32.0 / 7.0
        ));
        assert!(approx(
            opt_of("stats::sample_stddev([2, 4, 4, 4, 5, 5, 7, 9])"),
            (32.0f64 / 7.0).sqrt()
        ));
        assert_eq!(opt_of("stats::sample_variance([7])"), None);
        assert_eq!(opt_of("stats::sample_stddev([])"), None);
    }

    #[test]
    fn percentile_interpolates_between_ranks() {
        assert_eq!(opt_of("stats::percentile([1, 2, 3, 4], 0.0)"), Some(1.0));
        assert_eq!(opt_of("stats::percentile([1, 2, 3, 4], 100.0)"), Some(4.0));
        assert_eq!(opt_of("stats::percentile([1, 2, 3, 4], 50.0)"), Some(2.5));
        assert!(approx(opt_of("stats::percentile([1, 2, 3, 4], 90.0)"), 3.7));
        assert_eq!(opt_of("stats::percentile([], 50.0)"), None);
    }

    #[test]
    fn percentile_rejects_p_outside_the_range() {
        let error = call("stats::percentile([1, 2], 101.0)").unwrap_err();
        assert!(
            error.to_string().contains("expected p in 0..=100"),
            "{error}"
        );
    }

    /// The functions read the iterable; the caller's vector is still
    /// usable afterwards.
    #[test]
    fn functions_leave_the_caller_values_readable() {
        assert_eq!(
            u64_of("let v = [1, 2, 3]; stats::mean(v); stats::median(v); v.len()"),
            3
        );
    }

    #[test]
    fn to_f64_coerces_signed() {
        assert_eq!(to_f64(&rune::to_value(3i64).unwrap()).unwrap(), 3.0);
    }

    #[test]
    fn to_f64_coerces_float() {
        assert_eq!(to_f64(&rune::to_value(2.5f64).unwrap()).unwrap(), 2.5);
    }

    #[test]
    fn to_f64_rejects_string() {
        to_f64(&rune::to_value("nope").unwrap()).unwrap_err();
    }
}
