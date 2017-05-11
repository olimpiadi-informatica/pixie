# pixie
A system to boot multiple computers in multicast, supporting rsync-like updates

## How to use
To begin with, you should configure your network to boot with iPXE and load a
script via http. This is usually done by chainloading iPXE with the usual PXE
stack of your network card. If you are using dnsmasq as your DHCP server, you
can do this with the following configuration:

    # Detect if the request comes from ipxe or from the BIOS.
    dhcp-match=set:ipxe,175 # iPXE sends a 175 option.
    # If the request comes from the BIOS, load ipxe
    dhcp-boot=tag:!ipxe,undionly.kpxe
    # If PXE request comes from iPXE, direct it to boot from HTTP
    dhcp-boot=tag:ipxe,http://${next-server}/SCRIPT_URI

You can build everything by running `make`. This produces a kernel
image, two ramdisks and an ipxe image inside the folder `build/target`.

The URI received by pixied.py should be of the form `filename?ip=client_ip`,
where filename can be the empty string or `wipe` to delete data from the
hard drive. For example, you could use the following URL

    http://${next-server}/pixie/?ip=${ip}

supposing you have a nginx server with the following configuration block
listening on port 80 of the computer running dnsmasq:

    location /pixie/ {
        proxy_pass http://127.0.0.1:8123/;
    }

You also need to start `collector.py` to complete the configuration of the
clients. If run with the option `--wipe` this erases the `/etc/ethers` file
and starts building a new one, otherwise it appends data to the current one.
Otherwise it just append ethernet addresses to the current file.
To have the collector work as expected, you should add the following lines
to `nginx.conf`

    location /pixie_collector/ {
        proxy_pass http://127.0.0.1:8124/;
    }
