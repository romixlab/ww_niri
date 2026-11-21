//! A TV whose declared API hash differs from the client's, as a build from another crate version
//! would (P2618#21nov-2322): the shape is actually identical (it is the same `ww_niri_fake` build),
//! and introspection has to show the client that, instead of refusing every call on wire w0, the
//! design line.

use wire_weaver::ww_version::{ApiHash, ApiHashPair};
use ww_niri_fake::{ClockMode, FakeTv, Options};

use super::{connect, run};

#[test]
fn a_hash_that_only_the_version_moved_still_works() {
    let hash = ApiHashPair {
        no_docs: ApiHash::new(&[0xDE, 0xAD, 0xBE, 0xEF]),
        with_docs: ApiHash::new(&[0xDE, 0xAD, 0xBE, 0xEF]),
    };
    let tv = FakeTv::start(Options {
        clock: ClockMode::Manual,
        api_hash: Some(hash),
        ..Options::default()
    });
    run(async {
        let client = connect(&tv).await.unwrap();
        let info = client.info().call().await.unwrap();
        assert_eq!(info.hostname, "fake-tv");
    });
}
