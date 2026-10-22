//! `RsDataBaseBind` - one bind parameter of an `RsDataBasePrepare`.
//!
//! A bind is created by `RsDataBasePrepare::createBind()` (or implicitly by
//! `bind()` / `bindValue()`), configured through the fluent setters, and read
//! by `RsDataBasePrepare::execute()`.
//!
//! The Rust-side data lives in [`BindState`] behind `Rc<RefCell<..>>`, shared
//! between the PHP object and the owning statement (and the request registry,
//! which invalidates every bind in `RSHUTDOWN`).
//!
//! # Example (PHP)
//!
//! ```php
//! $q = $db->prepare('SELECT :id::int + 1 AS n');
//! $id = 41;
//! $q->createBind()->setName('id')->setValue($id)->setType(RsDataBasePrepare::INTEGER);
//! $q->execute();
//! ```

use crate::error::Error;
use crate::php::convert::{current_arg_reference, guard, is_known_bind_type, PhpResult};
use crate::php::exception::usage_error;
use ext_php_rs::flags::ClassFlags;
use ext_php_rs::prelude::*;
use ext_php_rs::types::{PhpRef, ZendClassObject, Zval};
use std::cell::RefCell;
use std::rc::Rc;

/// Shared mutable data of a bind parameter.
pub struct BindState {
    /// Normalized name without the leading `:`; `None` until `setName()`.
    pub name: Option<String>,
    /// By-reference zval (IN/OUT) or an owned copy (`bindValue`, IN only).
    pub value: Option<Zval>,
    /// `true` when `value` is a PHP reference that may receive OUT values.
    pub by_ref: bool,
    /// Declared size (`-1` = unspecified).
    pub size: i64,
    /// One of the `RsDataBasePrepare` type constants, `None` = auto-detect.
    pub type_name: Option<String>,
    /// Oracle type descriptor (`COLLECTION`).
    pub tdo: Option<String>,
    /// `false` after `removeBind()`, `free()` or `RSHUTDOWN`.
    pub attached: bool,
}

impl BindState {
    /// New attached bind without name or value.
    pub fn new() -> Self {
        Self {
            name: None,
            value: None,
            by_ref: false,
            size: -1,
            type_name: None,
            tdo: None,
            attached: true,
        }
    }

    /// Detach the bind and release the PHP value reference.
    ///
    /// Idempotent; called by `removeBind()`, `free()` and `RSHUTDOWN`.
    pub fn invalidate(&mut self) {
        self.attached = false;
        self.value = None;
    }
}

impl Default for BindState {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a bind name: `id` and `:id` are equivalent and stored as `id`.
///
/// # Example
///
/// ```ignore
/// assert_eq!(normalize_bind_name(":id").unwrap(), "id");
/// assert!(normalize_bind_name(":").is_err());
/// ```
pub fn normalize_bind_name(raw: &str) -> Result<String, Error> {
    let name = raw.trim().strip_prefix(':').unwrap_or(raw.trim());
    if name.is_empty() {
        return Err(Error::Bind("bind name must not be empty".into()));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '#')
    {
        return Err(Error::Bind(format!("invalid bind name '{}'", raw)));
    }
    Ok(name.to_string())
}

/// PHP class `RsDataBaseBind` (final). Created only via `RsDataBasePrepare::createBind()`.
#[php_class]
#[php(name = "RsDataBaseBind")]
#[php(flags = ClassFlags::Final)]
pub struct RsDataBaseBind {
    pub(crate) state: Rc<RefCell<BindState>>,
}

impl RsDataBaseBind {
    /// Wrap an existing shared state into a PHP-visible object value.
    pub fn from_state(state: Rc<RefCell<BindState>>) -> Self {
        Self { state }
    }
}

#[php_impl]
impl RsDataBaseBind {
    /// Private constructor: binds are created by `RsDataBasePrepare::createBind()`.
    ///
    /// ```php
    /// new RsDataBaseBind(); // Error: Call to private RsDataBaseBind::__construct()
    /// ```
    #[php(vis = "private")]
    pub fn __construct() -> PhpResult<Self> {
        Err(usage_error(
            "RsDataBaseBind can only be created by RsDataBasePrepare::createBind()",
        ))
    }

    /// Set the placeholder name; `'id'` and `':id'` are equivalent.
    ///
    /// ```php
    /// $bind->setName(':id'); // same as setName('id')
    /// ```
    pub fn set_name<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseBind>,
        name: String,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBaseBind>> {
        guard(|| {
            let normalized = normalize_bind_name(&name)?;
            self_.state.borrow_mut().name = Some(normalized);
            Ok(())
        })?;
        Ok(self_)
    }

    /// Bind a PHP variable by reference (IN / IN OUT). OUT values are written back
    /// only after a fully successful `execute()`.
    ///
    /// ```php
    /// $result = null;
    /// $bind->setValue($result); // after execute(): $result holds the OUT value
    /// ```
    #[allow(unused_variables)]
    pub fn set_value<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseBind>,
        value: PhpRef<'_>,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBaseBind>> {
        guard(|| {
            // SAFETY: called synchronously from this method body on the PHP thread;
            // `value` is argument #0.
            let reference = unsafe { current_arg_reference(0) }.ok_or_else(|| {
                usage_error("RsDataBaseBind::setValue() requires a variable passed by reference")
            })?;
            let mut st = self_.state.borrow_mut();
            st.value = Some(reference);
            st.by_ref = true;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Set the declared size (`-1` = unspecified).
    ///
    /// ```php
    /// $bind->setSize(4000);
    /// ```
    #[php(defaults(size = -1))]
    pub fn set_size<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseBind>,
        size: i64,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBaseBind>> {
        guard(|| {
            self_.state.borrow_mut().size = size;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Set the bind type (an `RsDataBasePrepare::*` constant) or `null` for auto-detection.
    ///
    /// ```php
    /// $bind->setType(RsDataBasePrepare::INTEGER);
    /// $bind->setType(null); // auto-detect from the PHP value
    /// ```
    pub fn set_type<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseBind>,
        r#type: Option<String>,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBaseBind>> {
        guard(|| {
            if let Some(t) = r#type.as_deref() {
                if !is_known_bind_type(t) {
                    return Err(usage_error(format!("unknown bind type '{}'", t)));
                }
            }
            self_.state.borrow_mut().type_name = r#type;
            Ok(())
        })?;
        Ok(self_)
    }

    /// Set the Oracle type descriptor used for `COLLECTION` binds (e.g. `'D_CL_ID'`).
    ///
    /// ```php
    /// $bind->setType(RsDataBasePrepare::COLLECTION)->setTdo('D_CL_ID');
    /// ```
    pub fn set_tdo<'a>(
        self_: &'a mut ZendClassObject<RsDataBaseBind>,
        tdo: Option<String>,
    ) -> PhpResult<&'a mut ZendClassObject<RsDataBaseBind>> {
        guard(|| {
            self_.state.borrow_mut().tdo = tdo;
            Ok(())
        })?;
        Ok(self_)
    }
}
