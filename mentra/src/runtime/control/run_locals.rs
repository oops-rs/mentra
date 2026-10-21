use std::{
    any::{Any, TypeId},
    collections::HashMap,
    sync::Arc,
};

/// Values a host attaches to one run, for the tools that run calls to read.
///
/// The per-run sibling of [`Runtime::app_context`](crate::Runtime::app_context).
/// An app context is the runtime's, so every run on it reads the same value —
/// right for a client handle, wrong for *who this run works for*. A host that
/// serves many callers from one runtime and one toolset has exactly that
/// second kind of fact: a caller's workspace, a tenant, a trace id. Without a
/// slot here it either opens a toolset per caller or keys a side table by
/// agent id; with one, it attaches the value to the run and the tool reads it
/// back through [`ToolContext::run_local`](crate::tool::ToolContext::run_local).
///
/// Keyed by type, at most one value per type, as `app_context` is: a host
/// wraps a bare `String` in a newtype of its own so two hosts' strings cannot
/// collide. Values are `Arc`ed and the map is copy-on-write, so cloning a
/// `RunOptions` — which the runner does freely — costs one reference count.
///
/// Inherited by [`RunOptions::child`](crate::runtime::RunOptions::child): a
/// delegated run works for the same caller as the run that delegated it, and
/// a tool the child calls has to see the same answer.
#[derive(Clone, Default)]
pub struct RunLocals {
    values: Arc<HashMap<TypeId, Entry>>,
}

#[derive(Clone)]
struct Entry {
    /// For `Debug`: a `dyn Any` cannot say what it is.
    type_name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
}

impl RunLocals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches `value`, replacing any value of the same type.
    #[must_use]
    pub fn with<T>(self, value: T) -> Self
    where
        T: Any + Send + Sync + 'static,
    {
        self.with_arc(Arc::new(value))
    }

    /// Attaches a value the host already shares, replacing any value of the
    /// same type.
    #[must_use]
    pub fn with_arc<T>(mut self, value: Arc<T>) -> Self
    where
        T: Any + Send + Sync + 'static,
    {
        Arc::make_mut(&mut self.values).insert(
            TypeId::of::<T>(),
            Entry {
                type_name: std::any::type_name::<T>(),
                value,
            },
        );
        self
    }

    /// The value of type `T`, when one is attached.
    pub fn get<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        let entry = self.values.get(&TypeId::of::<T>())?;
        Arc::clone(&entry.value).downcast::<T>().ok()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// `self` with every value of `over` attached on top: where both hold a
    /// type, `over`'s wins.
    #[must_use]
    pub fn layered(self, over: &RunLocals) -> Self {
        if over.is_empty() {
            return self;
        }
        if self.is_empty() {
            return over.clone();
        }
        let mut values = self.values;
        let map = Arc::make_mut(&mut values);
        for (type_id, entry) in over.values.iter() {
            map.insert(*type_id, entry.clone());
        }
        Self { values }
    }
}

/// Names the attached types, not their values: the values are the host's and
/// need not be `Debug`.
impl std::fmt::Debug for RunLocals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.values.values().map(|entry| entry.type_name).collect();
        names.sort_unstable();
        f.debug_set().entries(names).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Workspace(String);

    #[derive(Debug, PartialEq)]
    struct Tenant(u32);

    #[test]
    fn a_value_is_read_back_by_its_type() {
        let context = RunLocals::new()
            .with(Workspace("ws-1".into()))
            .with(Tenant(7));

        assert_eq!(
            context.get::<Workspace>().as_deref(),
            Some(&Workspace("ws-1".into()))
        );
        assert_eq!(context.get::<Tenant>().as_deref(), Some(&Tenant(7)));
        assert_eq!(context.get::<String>(), None);
    }

    #[test]
    fn a_second_value_of_a_type_replaces_the_first() {
        let context = RunLocals::new()
            .with(Workspace("old".into()))
            .with(Workspace("new".into()));

        assert_eq!(
            context.get::<Workspace>().as_deref(),
            Some(&Workspace("new".into()))
        );
    }

    #[test]
    fn a_clone_does_not_see_what_is_attached_after_it() {
        let base = RunLocals::new().with(Tenant(1));
        let extended = base.clone().with(Workspace("ws".into()));

        assert_eq!(base.get::<Workspace>(), None);
        assert!(extended.get::<Tenant>().is_some());
    }

    #[test]
    fn layering_keeps_both_and_prefers_the_upper_value() {
        let lower = RunLocals::new()
            .with(Tenant(1))
            .with(Workspace("lower".into()));
        let upper = RunLocals::new().with(Workspace("upper".into()));

        let layered = lower.layered(&upper);

        assert_eq!(layered.get::<Tenant>().as_deref(), Some(&Tenant(1)));
        assert_eq!(
            layered.get::<Workspace>().as_deref(),
            Some(&Workspace("upper".into()))
        );
    }

    #[test]
    fn debug_names_the_types_only() {
        let context = RunLocals::new().with(Workspace("secret".into()));

        let shown = format!("{context:?}");
        assert!(shown.contains("Workspace"));
        assert!(!shown.contains("secret"));
    }
}
