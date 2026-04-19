use anyhow::Result;
use crate::core::executor::{run_dotnet_test, RunOptions};
use crate::core::filter::build_filter;

pub fn execute(filter_input: Option<&str>, build: bool) -> Result<()> {
    let filter = build_filter(filter_input);
    
    let options = RunOptions {
        filter,
        no_build: !build,
    };
    
    run_dotnet_test(options)
}
