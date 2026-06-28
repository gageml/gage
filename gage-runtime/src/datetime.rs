use std::cmp::Ordering;
use std::time::SystemTime;

use chrono::{DateTime as ChronoDateTime, Datelike, Duration as ChronoDuration, Timelike, Utc};
use rune::alloc::fmt::TryWrite;
use rune::runtime::Formatter;
use rune::{Any, ContextError, Module, item};

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct DateTime {
    #[rune(skip)]
    inner: ChronoDateTime<Utc>,
}

impl rune::alloc::prelude::TryClone for DateTime {
    fn try_clone(&self) -> Result<Self, rune::alloc::Error> {
        Ok(self.clone())
    }
}

impl DateTime {
    pub(crate) fn from_system_time(t: SystemTime) -> Self {
        DateTime { inner: t.into() }
    }

    #[rune::function(keep, path = Self::from_millis)]
    pub(crate) fn from_millis(ms: i64) -> Self {
        let inner = ChronoDateTime::from_timestamp_millis(ms)
            .expect("epoch millis within chrono representable range");
        DateTime { inner }
    }

    #[rune::function(keep, path = Self::now)]
    pub(crate) fn now() -> Self {
        DateTime { inner: Utc::now() }
    }

    #[rune::function(keep, path = Self::from_rfc3339)]
    fn from_rfc3339(s: &str) -> Result<Self, String> {
        ChronoDateTime::parse_from_rfc3339(s)
            .map(|dt| DateTime {
                inner: dt.with_timezone(&Utc),
            })
            .map_err(|e| e.to_string())
    }

    #[rune::function(keep, path = Self::parse)]
    fn parse(s: &str, fmt: &str) -> Result<Self, String> {
        ChronoDateTime::parse_from_str(s, fmt)
            .map(|dt| DateTime {
                inner: dt.with_timezone(&Utc),
            })
            .map_err(|e| e.to_string())
    }

    #[rune::function(keep, instance)]
    fn to_rfc3339(&self) -> String {
        self.inner.to_rfc3339()
    }

    #[rune::function(keep, instance)]
    fn to_rfc2822(&self) -> String {
        self.inner.to_rfc2822()
    }

    #[rune::function(keep, instance)]
    fn millis(&self) -> i64 {
        self.inner.timestamp_millis()
    }

    #[rune::function(keep, instance)]
    fn timestamp(&self) -> i64 {
        self.inner.timestamp()
    }

    #[rune::function(keep, instance)]
    fn year(&self) -> i64 {
        self.inner.year() as i64
    }

    #[rune::function(keep, instance)]
    fn month(&self) -> i64 {
        self.inner.month() as i64
    }

    #[rune::function(keep, instance)]
    fn day(&self) -> i64 {
        self.inner.day() as i64
    }

    #[rune::function(keep, instance)]
    fn hour(&self) -> i64 {
        self.inner.hour() as i64
    }

    #[rune::function(keep, instance)]
    fn minute(&self) -> i64 {
        self.inner.minute() as i64
    }

    #[rune::function(keep, instance)]
    fn second(&self) -> i64 {
        self.inner.second() as i64
    }

    #[rune::function(keep, instance)]
    fn weekday(&self) -> i64 {
        self.inner.weekday().num_days_from_monday() as i64
    }

    #[rune::function(keep, instance)]
    fn weekday_name(&self) -> String {
        self.inner.weekday().to_string()
    }

    #[rune::function(keep, instance)]
    fn ordinal(&self) -> i64 {
        self.inner.ordinal() as i64
    }

    #[rune::function(keep, instance)]
    fn format(&self, fmt: &str) -> String {
        self.inner.format(fmt).to_string()
    }

    #[rune::function(keep, instance)]
    fn add(&self, d: &Duration) -> Self {
        DateTime {
            inner: self.inner + d.inner,
        }
    }

    #[rune::function(keep, instance)]
    fn sub(&self, d: &Duration) -> Self {
        DateTime {
            inner: self.inner - d.inner,
        }
    }

    #[rune::function(keep, instance)]
    fn duration_since(&self, other: &Self) -> Duration {
        Duration {
            inner: self.inner - other.inner,
        }
    }

    #[rune::function(keep, instance, protocol = ADD)]
    fn proto_add(&self, d: &Duration) -> Self {
        self.add(d)
    }

    #[rune::function(keep, instance, protocol = SUB)]
    fn proto_sub(&self, d: &Duration) -> Self {
        self.sub(d)
    }

    #[rune::function(keep, instance, protocol = PARTIAL_EQ)]
    fn partial_eq(&self, rhs: &Self) -> bool {
        PartialEq::eq(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = EQ)]
    fn eq(&self, rhs: &Self) -> bool {
        PartialEq::eq(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = PARTIAL_CMP)]
    fn partial_cmp(&self, rhs: &Self) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = CMP)]
    fn cmp(&self, rhs: &Self) -> Ordering {
        Ord::cmp(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = DISPLAY_FMT)]
    fn display_fmt(&self, f: &mut Formatter) -> rune::alloc::Result<()> {
        write!(f, "{}", self.inner.to_rfc3339())
    }

    #[rune::function(keep, instance, protocol = DEBUG_FMT)]
    fn debug_fmt(&self, f: &mut Formatter) -> rune::alloc::Result<()> {
        write!(f, "{}", self.inner.to_rfc3339())
    }
}

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Duration {
    #[rune(skip)]
    inner: ChronoDuration,
}

impl rune::alloc::prelude::TryClone for Duration {
    fn try_clone(&self) -> Result<Self, rune::alloc::Error> {
        Ok(self.clone())
    }
}

impl Duration {
    #[rune::function(keep, path = Self::milliseconds)]
    fn milliseconds(n: i64) -> Self {
        Duration {
            inner: ChronoDuration::milliseconds(n),
        }
    }

    #[rune::function(keep, path = Self::seconds)]
    fn seconds(n: i64) -> Self {
        Duration {
            inner: ChronoDuration::seconds(n),
        }
    }

    #[rune::function(keep, path = Self::minutes)]
    fn minutes(n: i64) -> Self {
        Duration {
            inner: ChronoDuration::minutes(n),
        }
    }

    #[rune::function(keep, path = Self::hours)]
    fn hours(n: i64) -> Self {
        Duration {
            inner: ChronoDuration::hours(n),
        }
    }

    #[rune::function(keep, path = Self::days)]
    fn days(n: i64) -> Self {
        Duration {
            inner: ChronoDuration::days(n),
        }
    }

    #[rune::function(keep, path = Self::weeks)]
    fn weeks(n: i64) -> Self {
        Duration {
            inner: ChronoDuration::weeks(n),
        }
    }

    #[rune::function(keep, instance)]
    fn as_millis(&self) -> i64 {
        self.inner.num_milliseconds()
    }

    #[rune::function(keep, instance)]
    fn as_seconds(&self) -> i64 {
        self.inner.num_seconds()
    }

    #[rune::function(keep, instance)]
    fn as_minutes(&self) -> i64 {
        self.inner.num_minutes()
    }

    #[rune::function(keep, instance)]
    fn as_hours(&self) -> i64 {
        self.inner.num_hours()
    }

    #[rune::function(keep, instance)]
    fn as_days(&self) -> i64 {
        self.inner.num_days()
    }

    #[rune::function(keep, instance, protocol = PARTIAL_EQ)]
    fn partial_eq(&self, rhs: &Self) -> bool {
        PartialEq::eq(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = EQ)]
    fn eq(&self, rhs: &Self) -> bool {
        PartialEq::eq(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = PARTIAL_CMP)]
    fn partial_cmp(&self, rhs: &Self) -> Option<Ordering> {
        PartialOrd::partial_cmp(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = CMP)]
    fn cmp(&self, rhs: &Self) -> Ordering {
        Ord::cmp(&self.inner, &rhs.inner)
    }

    #[rune::function(keep, instance, protocol = DISPLAY_FMT)]
    fn display_fmt(&self, f: &mut Formatter) -> rune::alloc::Result<()> {
        write!(f, "{}", self.inner)
    }

    #[rune::function(keep, instance, protocol = DEBUG_FMT)]
    fn debug_fmt(&self, f: &mut Formatter) -> rune::alloc::Result<()> {
        write!(f, "{}", self.inner)
    }
}

pub(crate) fn register_types(m: &mut Module) -> Result<(), ContextError> {
    m.ty::<DateTime>()?;
    m.function_meta(DateTime::from_millis__meta)?;
    m.function_meta(DateTime::now__meta)?;
    m.function_meta(DateTime::from_rfc3339__meta)?;
    m.function_meta(DateTime::parse__meta)?;
    m.function_meta(DateTime::to_rfc3339__meta)?;
    m.function_meta(DateTime::to_rfc2822__meta)?;
    m.function_meta(DateTime::millis__meta)?;
    m.function_meta(DateTime::timestamp__meta)?;
    m.function_meta(DateTime::year__meta)?;
    m.function_meta(DateTime::month__meta)?;
    m.function_meta(DateTime::day__meta)?;
    m.function_meta(DateTime::hour__meta)?;
    m.function_meta(DateTime::minute__meta)?;
    m.function_meta(DateTime::second__meta)?;
    m.function_meta(DateTime::weekday__meta)?;
    m.function_meta(DateTime::weekday_name__meta)?;
    m.function_meta(DateTime::ordinal__meta)?;
    m.function_meta(DateTime::format__meta)?;
    m.function_meta(DateTime::add__meta)?;
    m.function_meta(DateTime::sub__meta)?;
    m.function_meta(DateTime::duration_since__meta)?;
    m.function_meta(DateTime::proto_add__meta)?;
    m.function_meta(DateTime::proto_sub__meta)?;
    m.function_meta(DateTime::partial_eq__meta)?;
    m.implement_trait::<DateTime>(item!(::std::cmp::PartialEq))?;
    m.function_meta(DateTime::eq__meta)?;
    m.implement_trait::<DateTime>(item!(::std::cmp::Eq))?;
    m.function_meta(DateTime::partial_cmp__meta)?;
    m.implement_trait::<DateTime>(item!(::std::cmp::PartialOrd))?;
    m.function_meta(DateTime::cmp__meta)?;
    m.implement_trait::<DateTime>(item!(::std::cmp::Ord))?;
    m.function_meta(DateTime::display_fmt__meta)?;
    m.function_meta(DateTime::debug_fmt__meta)?;

    m.ty::<Duration>()?;
    m.function_meta(Duration::milliseconds__meta)?;
    m.function_meta(Duration::seconds__meta)?;
    m.function_meta(Duration::minutes__meta)?;
    m.function_meta(Duration::hours__meta)?;
    m.function_meta(Duration::days__meta)?;
    m.function_meta(Duration::weeks__meta)?;
    m.function_meta(Duration::as_millis__meta)?;
    m.function_meta(Duration::as_seconds__meta)?;
    m.function_meta(Duration::as_minutes__meta)?;
    m.function_meta(Duration::as_hours__meta)?;
    m.function_meta(Duration::as_days__meta)?;
    m.function_meta(Duration::partial_eq__meta)?;
    m.implement_trait::<Duration>(item!(::std::cmp::PartialEq))?;
    m.function_meta(Duration::eq__meta)?;
    m.implement_trait::<Duration>(item!(::std::cmp::Eq))?;
    m.function_meta(Duration::partial_cmp__meta)?;
    m.implement_trait::<Duration>(item!(::std::cmp::PartialOrd))?;
    m.function_meta(Duration::cmp__meta)?;
    m.implement_trait::<Duration>(item!(::std::cmp::Ord))?;
    m.function_meta(Duration::display_fmt__meta)?;
    m.function_meta(Duration::debug_fmt__meta)?;
    Ok(())
}
