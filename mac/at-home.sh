#!/bin/sh
# Exit 0 when the Mac is on the home network: default gateway is the home Deco router,
# identified by its MAC (an IP check alone would match any other Deco network).
HOME_GW_MAC=$(cat ~/.config/kale/home-gw-mac 2>/dev/null)   # file holds the home router's MAC
[ -n "$HOME_GW_MAC" ] || exit 1
gw=$(/sbin/route -n get default 2>/dev/null | awk '/gateway:/{print $2}')
[ -n "$gw" ] || exit 1
mac=$(/usr/sbin/arp -n "$gw" 2>/dev/null | awk '{print $4}')
[ "$mac" = "$HOME_GW_MAC" ]
