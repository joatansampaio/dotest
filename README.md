# dotest

**dotest** is a fast, ergonomic, and interactive terminal user interface (TUI) for running .NET tests. It visually discovers and presents your C# tests as an interactive tree, allowing you to seamlessly run individual tests, methods, classes, or entire projects with immediate real-time feedback.

Instead of wading through verbose `dotnet test` console outputs and manually writing filters like `dotnet test --filter "FullyQualifiedName~MyClass"`, `dotest` does the heavy lifting for you visually within your terminal.

## Installation / Building from Source

Ensure you have [Rust and Cargo](https://rustup.rs/) installed to compile the application.

1. Clone or download the repository:
   ```bash
   git clone https://github.com/joatansampaio/dotest.git
   cd dotest
   ```

2. Build and install it globally using cargo:
   ```bash
   cargo install --path .
   ```
   *(This will compile the application and place `dotest.exe` in your `.cargo/bin` folder, which should be in your PATH.)*

---

## Usage

Navigate to any `.NET` project or solution directory containing tests and simply run:

```bash
dotest ui
```

**dotest** will automatically scan your project, discover your tests, and drop you into the interactive UI.

### Navigation and Commands

Once you are in the application, use the following shortcuts:

#### Navigation
- **↑ / ↓** : Move selection up and down.
- **← / →** : Collapse / Expand directories or classes.
- **a-z/0-9** : Start typing to dynamically filter/search tests.
- **Backspace** : Delete a search character.
- **Esc** : Clear your active search filter.

#### Execution & Toggles
- **Space** : Select / Deselect the highlighted test or folder.
- **Ctrl+A** : Toggle entirely all visible tests.
- **Enter** : Run the currently selected tests.
- **Esc** : Cancel an actively running `.NET` test execution.

#### Options & Configuration
- **Ctrl+P** : Open settings (toggles for skipping build, verbosity logic, caching optimizations).
- **F5** : Manually rediscover and refresh tests from the hard drive.
- **? or F1** : View the Help Dialog.

### Configurations

When hitting **Ctrl+P**, `dotest` opens a settings modal allowing you to persist standard behaviors:

- **Skip build:** Avoids recompiling on test runs.
- **Verbosity Modes:** Lets you manage the amount of `dotnet test` logs piped to the output pane without cluttering MSBuild output.
- **Cache discovered tests:** Stops `dotest` from having to rescan the project every startup. It saves the existing tree into `.dotest_cache.json`. Use **F5** to forcefully renew the cache.

All of these UI settings are saved persistently into `.dotest.yml` in your current directory.
