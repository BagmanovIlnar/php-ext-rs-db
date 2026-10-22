#![cfg_attr(windows, feature(abi_vectorcall))]
//! `rs_database_connect`: Rust core (dsn, pool, drivers, SQL parser) plus the PHP
//! extension bindings in [`php`].

pub mod bind_value;
pub mod config;
pub mod driver;
pub mod dsn;
pub mod error;
pub mod php;
pub mod pool;
pub mod sql;

use crate::php::bind::RsDataBaseBind;
use crate::php::connect::RsDataBaseConnect;
use crate::php::cursor::RsDataBaseCursor;
use crate::php::lifecycle::{minfo, minit, mshutdown, rinit, rshutdown};
use crate::php::prepare::RsDataBasePrepare;
use ext_php_rs::prelude::*;

/// Module entry point: classes, request/module lifecycle hooks, `phpinfo()` section.
///
/// Classes are registered in dependency order (the base exception before its children).
#[php_module]
#[php(startup = "minit")]
pub fn get_module(module: ModuleBuilder) -> ModuleBuilder {
    module
        .class::<RsDataBaseConnect>()
        .class::<RsDataBasePrepare>()
        .class::<RsDataBaseBind>()
        .class::<RsDataBaseCursor>()
        .request_startup_function(rinit)
        .request_shutdown_function(rshutdown)
        .shutdown_function(mshutdown)
        .info_function(minfo)
}
