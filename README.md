# pixie v2
A system to boot multiple computers, supporting rsync-like updates

## Usage
* Install the required dependencies
  ```sh
  yay -S rustup upx trunk
  rustup toolchain install stable
  rustup target add x86_64-unknown-uefi
  ```
* run `./setup.sh` to compile pixie and prepare the `storage` directory.
* modify the configuration file at `storage/config.yaml`.
* run with root privilegies the server `./pixie-server/target/release/pixie-server`.
