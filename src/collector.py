#!/usr/bin/env python3

import os
import sys
import traceback
import re
import argparse
import ipaddress
import time
import threading
from subprocess import call

from colorama import init, Fore, Style
from flask import Flask, request

from werkzeug.exceptions import (BadRequest, HTTPException,
                                 InternalServerError, NotFound)
from werkzeug.routing import Map, Rule, RequestRedirect
from werkzeug.wrappers import Request, Response
from werkzeug.wsgi import responder


class EthersManager():
    def __init__(self, ethers_path, static_ethers, wipe, ipv4):
        """
        :param ethers_path: path to the ethers file
        :param static_ethers: path to static ethers file (only when wiping)
        :param wipe: wipe the ethers
        """
        self.ethers_path = ethers_path
        self.wipe = wipe
        self.ethers = {}
        self.ipv4 = ipv4

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
            print(Fore.RED + Style.BRIGHT + "ERROR: %s is not writable" %
                  path, file=sys.stderr)
            sys.exit(1)

    @staticmethod
    def assert_readable(path):
        if not os.access(path, os.R_OK):
            print(Fore.RED + Style.BRIGHT + "ERROR: %s is not readable" %
                  path, file=sys.stderr)
            sys.exit(1)

    def load_ethers(self, path):
        EthersManager.assert_readable(path)
        lines = open(path, 'r').readlines()

        print(Fore.BLUE + "The ethers file is")
        print(''.join(lines))

        for line in lines:
            if self.ipv4:
                pieces = line.strip().split(' ')
            else:
                try:
                    pieces = line.strip()
                    groups = re.findall(
                        r".*((?:[0-9a-f]{2}:){5}[0-9a-f]{2}).*\[([^]]+)\]", pieces)
                    if len(groups) != 1:
                        continue
                    pieces = groups[0]
                except:
                    continue

            if len(pieces) != 2:
                continue

            mac, ip = pieces
            self.ethers[mac] = ip

    @staticmethod
    def check_mac_format(mac):
        if not re.fullmatch('([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}', mac):
            return False
        return True

    def check_ip_format(self, ip):
        try:
            if self.ipv4:
                ipaddress.IPv4Address(ip)
            else:
                ipaddress.IPv6Address(ip)
        except:
            return False
        return True

    def add_ether(self, mac, ip):
        if not EthersManager.check_mac_format(mac):
            return 'Invalid MAC: ' + mac
        if not self.check_ip_format(ip):
            return 'Invalid IP: ' + ip
        if mac in self.ethers:
            return 'MAC already present!'
        if ip in self.ethers.values():
            return 'IP already present'

        self.ethers[mac] = ip
        return None

    def print_boxed(self, lines):
        if len(lines) == 0:
            return

        lun = max([len(line) for line in lines])
        print('-' * (lun+4))
        for line in lines:
            print('| %s |' % (line.ljust(lun)))
        print('-' * (lun+4))

    def export_ethers(self):
        if self.ipv4:
            lines = ["%s %s" % (mac, ip) for mac, ip in self.ethers.items()]
        else:
            lines = ["%s,[%s],5m" %
                     (mac, ip) for mac, ip in self.ethers.items()]

        print(Fore.GREEN + Style.BRIGHT + "Generated ethers file")
        self.print_boxed(lines)

        file = open(self.ethers_path, 'w')
        file.write('\n'.join(lines) + '\n')
        file.close()

        print(Fore.GREEN + "%s file written" % self.ethers_path)

    def reload_services(self):
        print(Fore.GREEN + Style.BRIGHT + "Reloading services")

        print(Fore.GREEN + "Reloading dnsmasq")
        call(['systemctl', 'restart', 'dnsmasq.service'])


class ScriptHandler(object):

    def __init__(self, contestant_ip_format, worker_ip_format, reboot_delay, ethers_manager):
        self.contestant_ip_format = contestant_ip_format
        self.worker_ip_format = worker_ip_format
        self.reboot_delay = reboot_delay
        self.ethers_manager = ethers_manager
        self.reboot_string = 0

    def add_contestant(self, mac, row, col):
        try:
            if not 1 <= int(row) <= 255:
                raise ValueError()
            if not 1 <= int(col) <= 255:
                raise ValueError()
        except:
            return "Invalid row/col: row=%s col=%s" % (row, col), 400

        ip = self.contestant_ip_format.replace('R', row).replace('C', col)

        print(Fore.CYAN + "Contestant PC connected: MAC=%s IP=%s" % (mac, ip))
        result = self.ethers_manager.add_ether(mac, ip)
        if result:
            print(Fore.RED + result)
            return result, 400
        else:
            return ip

    def add_worker(self, mac, num):
        try:
            if not 1 <= int(num) <= 255:
                raise ValueError()
        except:
            return "Invalid num: num=%s" % (num), 400

        ip = self.worker_ip_format.replace('N', num)

        print(Fore.BLUE + "Worker PC connected: MAC=%s IP=%s" % (mac, ip))
        result = self.ethers_manager.add_ether(mac, ip)
        if result:
            print(Fore.RED + result)
            return result, 400
        else:
            return ip


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='pixie ip collector')
    parser.add_argument(
        '-c', '--contestant', help='Contestant IP format, default: fdcd::c:R:C', default='fdcd::c:R:C')
    parser.add_argument(
        '-w', '--worker', help='Worker IP format, default: fdcd::d:0:N', default='fdcd::d:0:N')
    parser.add_argument('-s', '--static', help='Path to static ethers')
    parser.add_argument('-4', '--ipv4', action='store_true', help='IPv4 mode')
    parser.add_argument('--wipe', help='Wipe ethers file and start from static (or from scratch)',
                        action='store_true', default=False)
    parser.add_argument(
        '-e', '--ethers', help='Path to ethers file, default: /etc/dnsmasq.d/ethers.conf', default='/etc/dnsmasq.d/ethers.conf')
    parser.add_argument(
        '-l', '--listen', help='Address to listen to, default: ::', default='::')
    parser.add_argument(
        '-p', '--port', help='Port to listen to, default: 8124', default=8124, type=int)
    parser.add_argument('-r', '--reboot-delay',
                        help='Delay between reboot requests', default=30, type=float)
    args = parser.parse_args()

    init(autoreset=True)

    ethers_manager = EthersManager(
        args.ethers, args.static, args.wipe, args.ipv4)
    script_handler = ScriptHandler(args.contestant, args.worker,
                                   args.reboot_delay, ethers_manager)
    reboot_string = 0

    app = Flask(__name__)

    @app.route("/contestant", methods=["GET"])
    def GET_contestant():
        args = request.args
        if 'mac' not in args or 'row' not in args or 'col' not in args:
            return 'Required query parameters: mac, row, col', 400
        else:
            mac = args['mac']
            row = args['row']
            col = args['col']
            return script_handler.add_contestant(mac, row, col)

    @app.route("/worker", methods=["GET"])
    def GET_worker():
        args = request.args
        if 'mac' not in args or 'num' not in args:
            return 'Required query parameters: mac, num', 400
        else:
            mac = args['mac']
            num = args['num']
            return script_handler.add_worker(mac, num)

    @app.route("/reboot_timestamp", methods=["GET"])
    def GET_reboot_timestamp():
        return str(reboot_string)

    def reboot_loop():
        global reboot_string
        while True:
            ethers_manager.export_ethers()
            print(Fore.YELLOW + "Reboot index %d" % reboot_string)
            reboot_string += 1
            PRE_SLEEP = 2
            print(Fore.YELLOW +
                  f"Wating {PRE_SLEEP}s before restarting dnsmasq")
            time.sleep(PRE_SLEEP)
            ethers_manager.reload_services()
            time.sleep(args.reboot_delay - PRE_SLEEP)

    threading.Thread(target=reboot_loop).start()
    app.run(threaded=True, host=args.listen, port=args.port)
