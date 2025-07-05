# pixie v2
A system to boot multiple computers, supporting rsync-like updates

## Usage
* Install the required dependencies
  ```sh
  yay -S rustup
  rustup toolchain install stable nightly
  rustup target add x86_64-unknown-uefi
  yay -S upx trunk
  rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-gnu
  ```
* run `./setup.sh` to compile pixie and prepare the `storage` directory.
* modify the configuration file at `storage/config.yaml`.
* run with root privilegies the server `./pixie-server/target/release/pixie-server`.
