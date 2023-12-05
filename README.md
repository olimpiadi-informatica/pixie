# pixie v2
A system to boot multiple computers, supporting rsync-like updates

## Usage
* Install the required dependencies
  ```sh
  yay -S rustup
  rustup toolchain install stable nightly
  yay -S upx trunk
  rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
  ```
* run `./setup.sh` to compile pixie and prepare the `storage` directory.
* modify the configuration file at `storage/config.yaml`.
* run with root privilegies the server `./pixie-core/target/release/pixie-server`.

## TODO
* progress bar per up/download
* versioning delle immagini
* supporto ad altri FS (fat, btrfs, ?)
* UI
* gruppi più flessibili
* registrazione più fancy
* current/next action, abort operation
* SIGHUP
* chunks garbage collection
* trovare il leak di file descriptors
* trovare in automatico l'IP per proxy
