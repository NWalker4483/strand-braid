use eyre::Result;

lazy_static::lazy_static! {
    static ref NOKHWA_MODULE: ci2_nokhwa::WrappedModule = ci2_nokhwa::new_module().unwrap();
}

fn main() -> Result<()> {
    let guard = ci2_nokhwa::make_singleton_guard(&&*NOKHWA_MODULE)?;
    let mymod = ci2_async::into_threaded_async(&*NOKHWA_MODULE, &guard);
    strand_cam::cli_app::cli_main(mymod, env!("CARGO_PKG_NAME"))?;
    Ok(())
}
