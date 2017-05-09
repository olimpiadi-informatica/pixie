#!/usr/bin/env python3

import os
import sys
import traceback
import argparse

try:
    from colorama import init, Fore, Style
except:
    # colorama is not really required... but is cool to have
    print('### colorama package is not required but you suck if you not install!')
    def nope(autoreset=False):
        pass
    class AttrDict(dict):
        def __init__(self, *args, **kwargs):
            super(AttrDict, self).__init__(*args, **kwargs)
            self.__dict__ = self
    Fore = AttrDict({ 'GREEN': '-- ', 'RED': '## ', 'BLUE': '++ ', 'CYAN': '== ', 'YELLOW': '** ' })
    Style = AttrDict({ 'BRIGHT': '' })
    init = nope

import gevent
import gevent.wsgi

from gevent import monkey
from werkzeug.exceptions import (BadRequest, HTTPException,
                                 InternalServerError, NotFound)
from werkzeug.routing import Map, Rule, RequestRedirect
from werkzeug.wrappers import Request, Response
from werkzeug.wsgi import responder

from subprocess import call

monkey.patch_all()


REBOOT_DELAY = 30


class EthersManager():
    def __init__(self, ethers_path, num_ethers, static_ethers, wipe):
        self.ethers_path = ethers_path
        self.remaining_ethers = num_ethers
        self.wipe = wipe
        self.ethers = []

        EthersManager.assert_writable(self.ethers_path)

        if wipe:
            print(Fore.RED + Style.BRIGHT + "The ethers file will be wiped!")
            if static_ethers:
                print(Fore.BLUE + "The static ethers will be loaded")
                self.load_ethers(static_ethers)
            else:
                print(Fore.BLUE + "The ethers file will be created from scratch")
        else:
            self.load_ethers(self.ethers_path)

    @staticmethod
    def assert_writable(path):
        if not os.access(path, os.W_OK):
            print(Fore.RED + Style.BRIGHT + "ERROR: %s is not writable" % path, file=sys.stderr)
            sys.exit(1)

    @staticmethod
    def assert_readable(path):
        if not os.access(path, os.R_OK):
            print(Fore.RED + Style.BRIGHT + "ERROR: %s is not readable" % path, file=sys.stderr)
            sys.exit(1)

    def load_ethers(self, path):
        EthersManager.assert_readable(path)
        lines = open(path, 'r').readlines()
        print(Fore.BLUE + "The ethers file is")
        print(''.join(lines))

        for line in lines:
            pieces = line.split(' ')
            if len(pieces) != 2: continue

            mac, ip = pieces
            self.ethers.append((mac, ip))

    def add_ether(self, mac, ip):
        self.ethers.append((mac, ip))
        self.remaining_ethers -= 1
        if self.remaining_ethers == 0:
            self.export_ethers()

    def export_ethers(self):
        lines = ''.join("%s %s\n" % (mac, ip) for mac,ip in self.ethers)
        print(Fore.GREEN + Style.BRIGHT + "Generated ethers file")
        print(lines)

        file = open(self.ethers_path, 'w')
        file.write(lines)
        file.close()

        print(Fore.GREEN + Style.BRIGHT + "%s file written" % self.ethers_path)

        self.reload_services()

    def reload_services(self):
        print(Fore.GREEN + Style.BRIGHT + "Reloading services")

        print(Fore.GREEN + "SIGHUPing dnsmasq")
        call(['killall', '-s', 'SIGHUP', 'dnsmasq'])


class ScriptHandler(object):

    def __init__(self, contestant_ip_format, worker_ip_format, ethers_manager):
        self.contestant_ip_format = contestant_ip_format
        self.worker_ip_format = worker_ip_format
        self.ethers_manager = ethers_manager
        self.reboot_string = 0

        gevent.spawn(self.reboot_loop)

        self.router = Map([
            Rule('/contestant', methods=['GET'], endpoint='contestant'),
            Rule('/worker', methods=['GET'], endpoint='worker'),
            Rule('/reboot_timestamp', methods=['GET'], endpoint='reboot_timestamp')
        ])

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
        args = request.args
        response = Response()
        response.mimetype = 'text/plain'
        response.status_code = 200

        if endpoint == 'contestant':
            mac = args['mac']
            row = args['row']
            col = args['col']
            ip = self.contestant_ip_format.replace('R', row).replace('C', col)

            print(Fore.CYAN + "Contestant PC connected: MAC=%s IP=%s" % (mac, ip))
            self.ethers_manager.add_ether(mac, ip)
            response.data = ip

        elif endpoint == 'worker':
            mac = args['mac']
            num = args['num']
            ip = self.worker_ip_format.replace('N', num)

            print(Fore.BLUE + "Worker PC connected: MAC=%s IP=%s" % (mac, ip))
            self.ethers_manager.add_ether(mac, ip)
            response.data = ip

        elif endpoint == 'reboot_timestamp':
            response.data = str(self.reboot_string)

        return response

    def reboot_loop(self):
        while True:
            print(Fore.YELLOW + "Reboot index %d" % self.reboot_string)
            self.reboot_string += 1
            gevent.sleep(REBOOT_DELAY)

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='pixie ip collector')
    parser.add_argument('-c', '--contestant', help='Contestant IP format, default: 172.16.C.R', default='172.16.C.R')
    parser.add_argument('-w', '--worker', help='Worker IP format, default: 172.17.1.N', default='172.17.1.N')
    parser.add_argument('-s', '--static', help='Path to static ethers')
    parser.add_argument('--wipe', help='Wipe ethers file and start from static (or from scratch)',
                        action='store_true', default=False)
    parser.add_argument('-e', '--ethers', help='Path to ethers file, default: /etc/ethers', default='/etc/ethers')
    parser.add_argument('-n', '--num', help='Stop the script after N ethers received', default=-1, type=int)
    parser.add_argument('-l', '--listen', help='Address to listen to, default: 0.0.0.0', default='0.0.0.0')
    parser.add_argument('-p', '--port', help='Port to listen to, default: 8080', default=8080, type=int)
    args = parser.parse_args()

    init(autoreset=True)

    ethersManager = EthersManager(args.ethers, args.num, args.static, args.wipe)
    server = gevent.wsgi.WSGIServer((args.listen, args.port), ScriptHandler(args.contestant, args.worker, ethersManager))
    gevent.spawn(server.serve_forever).join()
