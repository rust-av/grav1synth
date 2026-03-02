coverage:
    cargo llvm-cov --lcov --output-path=lcov.info
    genhtml lcov.info --dark-mode --flat --missed --output-directory target/coverage_html

precommit:
    cargo fmt
    cargo clippy
    cargo test
