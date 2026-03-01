# Contributing to The Tweed

Thank you for your interest in contributing to The Tweed! I welcome bug reports, feature requests, and code contributions from the community.

## Contributor License Agreement (CLA)

Before I can accept your contribution, you must agree to our Contributor License Agreement. By submitting a pull request, you acknowledge that you have read and agree to the terms outlined in [CLA.md](CLA.md).

**Why a CLA?** The CLA ensures that:
- You grant necessary rights to use and distribute your contributions under GPLv3
- You represent that your contribution is your original work
- The project can be maintained and improved over time

## Build Requirements

### Prerequisites

- **Rust**: 1.92 or later (`rustup update stable`)
- **Cargo**: Comes with Rust installation
- **System Dependencies**:
  - CLAP and VST3 development headers (for plugin bundling)
  - See Dockerfile for dependencies

### Important Note: neampmod-engine Dependency

This plugin depends on `neampmod-engine`, which is not yet publicly available. You will **not be able to build** the plugin from source until the engine is released. However, you can still:
- Review the code for transparency and security
- Contribute improvements to the plugin-level code
- Report bugs and suggest features
- Improve documentation

When the engine is released, I will update the build instructions accordingly.

## Bug Reports

When reporting bugs, please include:

### Bug Report Template

```markdown
**Describe the bug**
A clear description of what the bug is.

**To Reproduce**
Steps to reproduce the behavior:
1. Load plugin in [DAW name]
2. Set [control] to [value]
3. Play guitar
4. Observe [unexpected behavior]

**Expected behavior**
What you expected to happen.

**System Information:**
- OS: [e.g., Ubuntu 24.04]
- Plugin Format: [VST3, CLAP]
- DAW: [e.g., Reaper 7.6, Ardour 8.12]
- Sample Rate: [e.g., 48kHz]
- Buffer Size: [e.g., 512 samples]
- CPU: [e.g., AMD Ryzen 5500U]

**Additional context**
Any other relevant information, screenshots, or audio examples.
```

## Feature Requests

When requesting features, please include:

### Feature Request Template

```markdown
**Is your feature request related to a problem?**
A clear description of the problem.

**Describe the solution you'd like**
A clear description of what you want to happen.

**Describe alternatives you've considered**
Other approaches you've thought about.

**Additional context**
Any other context, mockups, or examples.
```

## Questions and Discussions

- **General questions**: Open a GitHub Discussion
- **Technical questions**: Check existing issues or open a new one

## License

By contributing, you agree that your contributions will be licensed under the GNU General Public License v3.0 or later, matching the project's license.

---

Thank you for contributing to The Tweed!
