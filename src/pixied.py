#!/usr/bin/env python3

import argparse
import ipaddress
import gevent
import gevent.wsgi
import hashlib
import json
import traceback

from gevent import monkey
from werkzeug.exceptions import (BadRequest, HTTPException,
                                 InternalServerError, NotFound)
from werkzeug.routing import Map, Rule, RequestRedirect
from werkzeug.wrappers import Request, Response
from werkzeug.wsgi import responder

monkey.patch_all()

IMAGE_METHOD = 'tftp'

BOOTSCRIPT = """#!ipxe

:retry
dhcp && isset ${{filename}} || goto retry

echo Booting from ${{filename}}
kernel {image_method}://${{next-server}}/vmlinuz.img quiet pixie_server=${{next-server}} \
    ip=${{ip}}::${{gateway}}:${{netmask}}::eth0:none:${{dns}} {wipe} pixie_root_size={root_size} \
    pixie_swap_size={swap_size} pixie_sha224={sha224} {extra_args} || goto error
initrd {image_method}://${{next-server}}//initrd.img || goto error
boot || goto error

error:
shell
"""

CONFIGSCRIPT = """#!ipxe

:retry
dhcp && isset ${{filename}} || goto retry

echo Booting from ${{filename}}
kernel {image_method}://${{next-server}}/vmlinuz.img quiet \
    ip=${{ip}}::${{gateway}}:${{netmask}}::eth0:none:${{dns}} \
    SERVER_IP=${{next-server}}{collector_prefix} || goto error
initrd {image_method}://${{next-server}}//doconfig.img || goto error
boot || goto error

error:
shell
"""

class ScriptHandler(object):
    def __init__(self, configs, collector_prefix):
        self.configs = []
        self.default_config = dict()
        self.default_config['image_method'] = IMAGE_METHOD
        self.default_config['collector_prefix'] = collector_prefix
        for config in configs:
            self.configs.append(self.load_config(config))
        self.router = Map([
            Rule('/', methods=['GET'], endpoint='default'),
            Rule('/wipe', methods=['GET'], endpoint='wipe')
        ])

    def load_config(self, config):
        with open(config) as c:
            cfg = json.load(c)
        m = hashlib.sha224()
        m.update(bytes(cfg['subnet'], 'utf-8'))
        m.update(bytes(cfg['swap_size']))
        m.update(bytes(cfg['root_size']))
        m.update(bytes(cfg['extra_args'], 'utf-8'))
        # TODO: check sizes
        for f in cfg['hashes']:
            with open(f, 'rb') as fl:
                for line in fl:
                    m.update(line)
        cfg['sha224'] = m.hexdigest()
        cfg['subnet'] = ipaddress.ip_network(cfg['subnet'])
        cfg['image_method'] = IMAGE_METHOD
        return cfg

    @responder
    def __call__(self, environ, start_response):
        try:
            return self.wsgi_app(environ, start_response)
        except:
            traceback.print_exc()
            return InternalServerError()

    def wsgi_app(self, environ, start_response):
        route = self.router.bind_to_environ(environ)
        try:
            endpoint, args = route.match()
        except RequestRedirect as e:
            return e
        except HTTPException:
            return NotFound()

        request = Request(environ)
        get_args = dict(request.args)
        if endpoint == 'wipe':
            get_args['wipe'] = 'pixie_wipe=force'
        else:
            get_args['wipe'] = ""


        response = Response()
        response.mimetype = 'text/plain'
        response.status_code = 200

        config = None
        if 'ip' in get_args:
            ip_addr = ipaddress.ip_address(get_args['ip'][0])
            for cfg in self.configs:
                if ip_addr in cfg['subnet']:
                    config = cfg

        if config is None:
            response.data = CONFIGSCRIPT.format(**self.default_config)
        else:
            for (k, v) in config.items():
                get_args[k] = v
            response.data = BOOTSCRIPT.format(**get_args)
        return response

if __name__ == '__main__':
    parser = argparse.ArgumentParser(
        description="pixied",
        formatter_class=argparse.RawDescriptionHelpFormatter)

    parser.add_argument("configs", action="store", type=str, nargs="+",
                        help="config files to load")
    parser.add_argument("-a", "--addr", action="store", type=str, default="0.0.0.0",
                        help="address to bind to (default '0.0.0.0')")
    parser.add_argument("-p", "--port", action="store", type=int, default=8080,
                        help="port to bind to (default 8080)")
    parser.add_argument("-c", "--collector-prefix", action="store", type=str, default="/pixie_collector",
                        help="prefix on which the collector is served")
    args = parser.parse_args()
    server = gevent.wsgi.WSGIServer(
        (args.addr, args.port), ScriptHandler(args.configs, args.collector_prefix))
    gevent.spawn(server.serve_forever).join()
    
