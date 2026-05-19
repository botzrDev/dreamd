# Changelog

All notable changes to dreamd are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `rmcp 1.7.0` (MCP spec 2025-11-25) added as workspace dependency; consumed by downstream `dreamd mcp` subcommand (WEG-77).
- Initial project scaffold (Cargo workspace placeholder, SPEC.md, PRD, CONTRIBUTING.md).
- GitHub Actions CI/CD pipeline: lint, test, cross-platform build, binary size gate (NFR-2), DCO sign-off check.
- Release workflow: cross-platform binary builds published to GitHub Releases on tag push.
