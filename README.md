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

`undionly.kpxe` can be found on the iPXE website, or compiled manually from
the source code. If the network you are working on has another DHCP server
running on it, iPXE could occasionally have problems loading the script.
You can solve this by compiling a version of `undionly.kpxe` that only accepts
DHCP offers from BOOTP server, for example by embedding the script
`contrib/bootp_only.ipxe`:

    make bin/undionly.kpxe EMBED=path/to/pixie/contrib/bootp_only.ipxe

The URI received by pixied should be of the form `filename?client_ip`,
where filename can be the empty string or `wipe-force`, `wipe-linux`,
`wipe-pixie` to delete data from the hard drive. For example, you could
use the following URL

    http://${next-server}/pixie/?${ip}

supposing you have a nginx server with the following configuration block
listening on port 80 of the computer running dnsmasq:

    location /pixie {
        proxy_pass http://127.0.0.1:PIXIE_HTTP_PORT/;
    }
