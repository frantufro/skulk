# TODO

## Localhost Support

- [ ] Detect package manager on localhost (apt-get, brew, dnf, pacman) instead of assuming Debian/Ubuntu. Currently `run_remote_setup` fails with "apt-get not found" on macOS. Should fall back gracefully or offer manual install instructions per platform.
