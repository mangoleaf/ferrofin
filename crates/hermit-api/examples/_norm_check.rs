//! throwaway
fn main() {
    for p in [
        "/System/Info",
        "/System/Info/Public",
        "/Users/AuthenticateByName",
        "/Users/Me",
        "/UserViews",
        "/Items",
        "/Items/{itemId}",
        "/Items/{itemId}/PlaybackInfo",
        "/Videos/{itemId}/stream",
        "/Items/{itemId}/Images/{imageType}",
    ] {
        println!("{p} => {}", hermit_api::routes::normalize_contract_path(p));
    }
}
