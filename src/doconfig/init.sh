#!/bin/sh

error() {
    echo $1
    while true
    do
        sh
    done
}

SERVER_IP="[fdcd::1]"

curl "http://""$SERVER_IP""/reboot_timestamp" &> /dev/null

MAC=$( ip addr | grep "global eth0" -B1 | head -n1 | cut -d ' ' -f6 )
[ -z "$MAC" ] && error "Cannot find MAC address"
IP="unknown"

dialog --defaultno --yesno "Am I a worker?" 5 19
if [ "$?" -ne "0" ]
then
    while true; do
        dialog --nocancel --inputbox "Enter row [1-255]:" 8 22 2> __row
        dialog --nocancel --inputbox "Enter column [1-255]:" 8 25 2> __col
        ROW=$(cat __row)
        COL=$(cat __col)
        curl "http://""$SERVER_IP""/contestant?mac=""$MAC""&row=""$ROW""&col=""$COL" > __ip 2> __error
        if [ "$?" -eq "0" ]; then
            IP=$(cat __ip)
            break
        else
            dialog --msgbox "Error: $(cat __error)" 6 40
        fi
    done
else
    while true; do
        dialog --nocancel --inputbox "Enter number [1-255]:" 8 25 2> __num
        NUM=$(cat __num)
        curl "http://""$SERVER_IP""/worker?mac=""$MAC""&num=""$NUM" > __ip 2> __error
        if [ "$?" -eq "0" ]; then
            IP=$(cat __ip)
            break
        else
            dialog --msgbox "Error: $(cat __error)" 6 40
        fi
    done
fi

curl "http://""$SERVER_IP""/reboot_timestamp" > __timestamp
if [ "$?" -ne "0" ]
then
    reboot || error "error rebooting"
fi
dialog --infobox "Done, waiting to reboot\nI am $IP" 4 27

while true
do
    sleep 1
    curl "http://""$SERVER_IP""/reboot_timestamp" > __timestamp_new
    if [ "$?" -ne "0" ]
    then
        reboot || error "Error rebooting"
    fi
    cmp __timestamp __timestamp_new 2>/dev/null >/dev/null
    if [ "$?" -ne "0" ]
    then
        reboot
    fi
done
