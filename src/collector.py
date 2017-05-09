#!/usr/bin/env python3

import os
import sys
import traceback
import re
import argparse

from colorama import init, Fore, Style

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


class EthersManager():
    def __init__(self, ethers_path, static_ethers, wipe):
        """
        :param ethers_path: path to the ethers file
        :param static_ethers: path to static ethers file (only when wiping)
        :param wipe: wipe the ethers
        """
        self.ethers_path = ethers_path
        self.wipe = wipe
        self.ethers = {}

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
            pieces = line.strip().split(' ')
            if len(pieces) != 2: continue

            mac, ip = pieces
            self.ethers[mac] = ip

    @staticmethod
    def check_mac_format(mac):
        if not re.fullmatch('([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}', mac):
            return False
        return True

    @staticmethod
    def check_ip_format(ip):
        pieces = ip.split('.')
        if len(pieces) != 4: return False
        try:
            if not 0 <= int(pieces[0]) <= 255: return False
            if not 0 <= int(pieces[1]) <= 255: return False
            if not 0 <= int(pieces[2]) <= 255: return False
            if not 0 <= int(pieces[3]) <= 255: return False
        except:
            return False
        return True

    def add_ether(self, mac, ip):
        if not EthersManager.check_mac_format(mac):
            return 'Invalid MAC: ' + mac
        if not EthersManager.check_ip_format(ip):
            return 'Invalid IP: ' + ip
        if mac in self.ethers:
            return 'MAC already present!'
        if ip in self.ethers.values():
            return 'IP already present'

        self.ethers[mac] = ip
        return None

    def print_boxed(self, lines):
        if len(lines) == 0: return

        lun = max([len(line) for line in lines])
        print('-' * (lun+4))
        for line in lines:
            print('| %s |' % (line.ljust(lun)))
        print('-' * (lun+4))

    def export_ethers(self):
        lines = ["%s %s" % (mac, ip) for mac,ip in self.ethers.items()]

        print(Fore.GREEN + Style.BRIGHT + "Generated ethers file")
        self.print_boxed(lines)

        file = open(self.ethers_path, 'w')
        file.write('\n'.join(lines) + '\n')
        file.close()

        print(Fore.GREEN + "%s file written" % self.ethers_path)

        self.reload_services()

    def reload_services(self):
        print(Fore.GREEN + Style.BRIGHT + "Reloading services")

        print(Fore.GREEN + "SIGHUPing dnsmasq")
        call(['killall', '-s', 'SIGHUP', 'dnsmasq'])


class ScriptHandler(object):

    def __init__(self, contestant_ip_format, worker_ip_format, reboot_delay, ethers_manager):
        self.contestant_ip_format = contestant_ip_format
        self.worker_ip_format = worker_ip_format
        self.reboot_delay = reboot_delay
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

    def add_contestant(self, mac, row, col, response):
        try:
            if not 1 <= int(row) <= 255: raise
            if not 1 <= int(col) <= 255: raise
        except:
            response.status_code = 400
            response.data = "Invalid row/col: row=%s col=%s" % (row, col)
            return

        ip = self.contestant_ip_format.replace('R', row).replace('C', col)

        print(Fore.CYAN + "Contestant PC connected: MAC=%s IP=%s" % (mac, ip))
        result = self.ethers_manager.add_ether(mac, ip)
        if result:
            print(Fore.RED + result)
            response.data = result
            response.status_code = 400
        else:
            response.data = ip

    def add_worker(self, mac, num, response):
        try:
            if not 1 <= int(num) <= 255: raise
        except:
            response.status_code = 400
            response.data = "Invalid num: num=%s" % (num)
            return

        ip = self.worker_ip_format.replace('N', num)

        print(Fore.BLUE + "Worker PC connected: MAC=%s IP=%s" % (mac, ip))
        result = self.ethers_manager.add_ether(mac, ip)
        if result:
            print(Fore.RED + result)
            response.data = result
            response.status_code = 400
        else:
            response.data = ip

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
            if 'mac' not in args or 'row' not in args or 'col' not in args:
                response.status_code = 400
                response.data = 'Required query parameters: mac, row, col'
            else:
                mac = args['mac']
                row = args['row']
                col = args['col']
                self.add_contestant(mac, row, col, response)

        elif endpoint == 'worker':
            if 'mac' not in args or 'num' not in args:
                response.status_code = 400
                response.data = 'Required query parameters: mac, num'
            else:
                mac = args['mac']
                num = args['num']
                self.add_worker(mac, num, response)

        elif endpoint == 'reboot_timestamp':
            response.data = str(self.reboot_string)

        return response

    def reboot_loop(self):
        while True:
            self.ethers_manager.export_ethers()
            print(Fore.YELLOW + "Reboot index %d" % self.reboot_string)
            self.reboot_string += 1
            gevent.sleep(self.reboot_delay)

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='pixie ip collector')
    parser.add_argument('-c', '--contestant', help='Contestant IP format, default: 172.16.C.R', default='172.16.C.R')
    parser.add_argument('-w', '--worker', help='Worker IP format, default: 172.17.1.N', default='172.17.1.N')
    parser.add_argument('-s', '--static', help='Path to static ethers')
    parser.add_argument('--wipe', help='Wipe ethers file and start from static (or from scratch)',
                        action='store_true', default=False)
    parser.add_argument('-e', '--ethers', help='Path to ethers file, default: /etc/ethers', default='/etc/ethers')
    parser.add_argument('-l', '--listen', help='Address to listen to, default: 0.0.0.0', default='0.0.0.0')
    parser.add_argument('-p', '--port', help='Port to listen to, default: 8080', default=8080, type=int)
    parser.add_argument('-r', '--reboot-delay', help='Delay between reboot requests', default=30, type=float)
    args = parser.parse_args()

    init(autoreset=True)

    ethersManager = EthersManager(args.ethers, args.static, args.wipe)
    server = gevent.wsgi.WSGIServer((args.listen, args.port), ScriptHandler(args.contestant, args.worker,
                                                                            args.reboot_delay, ethersManager))
    gevent.spawn(server.serve_forever).join()
