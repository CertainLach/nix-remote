# nix-remote

WIP

Run nix packages on remote machine, which has no nix installed

It works by copying package closure to remote machine, replacing nix prefix from /nix/store to /tmp/nixrm, and then launching it here over ssh

Because it works by replacing prefix in compiled binaries, no rebuilds are required, and most of the time everything works fine.

For example, to start neovim on ssh machine `neptune`:

```
nix-remote nixpkgs#neovim neptune -c nvim
```
