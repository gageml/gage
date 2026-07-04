use rune::runtime::{Object, Value};
use rune::{Any, ContextError, Module};

use crate::state::current_scan_ctx;

use super::datetime::DateTime;
use super::value::json_to_value;

pub(crate) fn register(m: &mut Module) -> Result<(), ContextError> {
    m.function("scan", scan).build()?;
    m.function("params", params).build()?;
    Ok(())
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();

    m.ty::<Session>()?;
    m.ty::<Scan>()?;
    m.function_meta(Scan::sessions)?;

    m.ty::<Sessions>()?;
    m.function_meta(Sessions::next__meta)?;
    m.function_meta(Sessions::nth__meta)?;
    m.function_meta(Sessions::size_hint__meta)?;
    m.function_meta(Sessions::len__meta)?;
    m.function_meta(Sessions::next_back__meta)?;
    m.implement_trait::<Sessions>(rune::item!(::std::iter::Iterator))?;
    m.implement_trait::<Sessions>(rune::item!(::std::iter::DoubleEndedIterator))?;

    super::cache::register_types(&mut m)?;
    super::datetime::register_types(&mut m)?;
    super::query::register_types(&mut m)?;
    super::error::register_types(&mut m)?;
    super::ignore::register_types(&mut m)?;
    super::db::register_types(&mut m)?;
    super::config::register_types(&mut m)?;
    Ok(m)
}

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Session {
    #[rune(get)]
    pub id: String,
    #[rune(get)]
    pub modified: DateTime,
}

#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct Scan {
    #[rune(get)]
    pub id: String,
    #[rune(skip)]
    pub session_list: Vec<Session>,
}

impl Scan {
    #[rune::function(instance)]
    fn sessions(&self) -> Sessions {
        Sessions::new(self.session_list.clone())
    }
}

// Double-ended iterator over a scan's selected sessions, yielding
// newest-modified first. Mirrors std::slice::Iter so scanners get the
// full Iterator surface (rev, collect, map, ...) via implement_trait.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct Sessions {
    #[rune(skip)]
    items: Vec<Session>,
    #[rune(skip)]
    front: usize,
    #[rune(skip)]
    back: usize,
}

impl Sessions {
    /// Sessions not yet consumed by iteration.
    pub(crate) fn remaining(&self) -> Vec<Session> {
        self.items
            .get(self.front..self.back)
            .map(<[Session]>::to_vec)
            .unwrap_or_default()
    }

    fn new(items: Vec<Session>) -> Self {
        let back = items.len();
        Sessions {
            items,
            front: 0,
            back,
        }
    }

    #[rune::function(instance, keep, protocol = NEXT)]
    fn next(&mut self) -> Option<Session> {
        if self.front == self.back {
            return None;
        }
        let value = self.items.get(self.front)?.clone();
        self.front = self.front.wrapping_add(1);
        Some(value)
    }

    #[rune::function(instance, keep, protocol = NTH)]
    fn nth(&mut self, n: usize) -> Option<Session> {
        let n = self.front.wrapping_add(n);
        if n >= self.back || n < self.front {
            return None;
        }
        let value = self.items.get(n)?.clone();
        self.front = n.wrapping_add(1);
        Some(value)
    }

    #[rune::function(instance, keep, protocol = SIZE_HINT)]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.back.wrapping_sub(self.front);
        (len, Some(len))
    }

    #[rune::function(instance, keep, protocol = LEN)]
    fn len(&self) -> usize {
        self.back.wrapping_sub(self.front)
    }

    #[rune::function(instance, keep, protocol = NEXT_BACK)]
    fn next_back(&mut self) -> Option<Session> {
        if self.front == self.back {
            return None;
        }
        self.back = self.back.wrapping_sub(1);
        let value = self.items.get(self.back)?.clone();
        Some(value)
    }
}

fn scan() -> Scan {
    let ctx = current_scan_ctx();
    Scan {
        id: ctx.run.scan_id.clone(),
        session_list: ctx
            .run
            .selected
            .iter()
            .map(|s| Session {
                id: s.id.clone(),
                modified: DateTime::from_system_time(s.mtime),
            })
            .collect(),
    }
}

fn params() -> Value {
    let ctx = current_scan_ctx();
    match &ctx.params {
        Some(json_val) => json_to_value(json_val),
        None => rune::to_value(Object::new()).unwrap(),
    }
}
