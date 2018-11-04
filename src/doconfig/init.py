#!/usr/bin/env python3

from subprocess import run, PIPE
import requests
import os, time

SERVER_IP = "[fdcd::1]"
mac = os.environ['MY_MAC']
ip = ''

if run("dialog --defaultno --yesno \"Am I a worker?\" 5 19", shell=True, stderr=PIPE).returncode != 0:
    while True:
        row = run("dialog --nocancel --inputbox \"Enter row [1-255]:\" 8 22", shell=True, stderr=PIPE).stderr.decode()
        col = run("dialog --nocancel --inputbox \"Enter column [1-255]:\" 8 25", shell=True, stderr=PIPE).stderr.decode()
        url = "http://" + SERVER_IP + "/collector/contestant?mac=" + mac + "&row=" + row + "&col=" + col
        r = requests.get(url)
        if r.status_code != 200:
            print("Error getting ip")
            print(r)
        else:
            ip = r.text
            print(ip)
            break
else:
    while True:
        num = run("dialog --nocancel --inputbox \"Enter number [1-255]:\" 8 22", shell=True, stderr=PIPE).stderr.decode()
        url = "http://" + SERVER_IP + "/collector/worker?mac=" + mac + "&num=" + num
        r = requests.get(url)
        if r.status_code != 200:
            print("Error getting ip")
            print(r)
        else:
            ip = r.text
            print(ip)
            break

run("dialog --infobox \"Done, waiting to reboot\nI am " + ip + "\" 4 27", shell=True)

ts = ''
r = requests.get("http://" + SERVER_IP + "/collector/reboot_timestamp")
if r.status_code != 200:
    run("reboot", shell=True)
else:
    ts = r.text

while True:
    r = requests.get("http://" + SERVER_IP + "/collector/reboot_timestamp")
    if r.status_code != 200 or ts != r.text:
        run("reboot", shell=True)
    ts = r.text
    time.sleep(1)

