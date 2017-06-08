extern crate futures;
extern crate tokio_core;
extern crate tokio_signal_catcher;

use futures::Future;

struct Sentinel(String);

impl Drop for Sentinel {
    fn drop(&mut self) {
        println!("{}", self.0)
    }
}

fn main() {
    {
        let _sentinel = Sentinel("Sentinel destructor called!".into());

        let mut core = tokio_core::reactor::Core::new().unwrap();
        let handle = core.handle();

        println!("Zzz...");
        core.run(tokio_signal_catcher::catch(&handle, {
            let duration = std::time::Duration::new(8, 0);
            tokio_core::reactor::Timeout::new(duration, &handle)
                .unwrap()
                .and_then(|()| {
                    println!("Timeout!");
                    Ok(())
                })
                .map_err(|e| panic!("{}", e))
        }))
    }.unwrap_or_else(|e| e.terminate());
}
