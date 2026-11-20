use std::path::Path;

pub const DATA_LINK: &str = "..data";

#[expect(
    clippy::todo,
    unused_variables,
    reason = "skeleton: daemon/watcher-core"
)]
pub fn root(entry: &Path) -> Option<&Path> {
    todo!()
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "skeleton: daemon/watcher-core"]
    fn the_root_is_the_directory_that_holds_the_data_link() {}
}
