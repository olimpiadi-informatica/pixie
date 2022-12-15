# pixie v2
A system to boot multiple computers, supporting rsync-like updates

## How to use
* run `./setup.sh` to prepare the `storage` directory.
* modify the configuration file at `storage/config.yaml`.
* run with root privilegies the server `./pixie-core/target/release/pixie-server`.

## TODO
* progress bar per up/download
* versioning delle immagini
* supporto ad altri FS (fat, swap, btrfs, ?)
* UI
* gruppi più flessibili
* registrazione più fancy
