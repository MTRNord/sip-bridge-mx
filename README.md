# Matrix SIP Bridge

An attempt to allow dial-in and dial-out using SIP PBX systems with webrtc support for matrix clients.

## Requirements

* A Matrix homeserver
* A SIP PBX system like Asterisk with webrtc+ws support
* A Matrix client that supports voice calls
* Rust

## Installation

```bash
cargo install --git https://github.com/MTRNord/sip-bridge-mx.git
```

## Usage

```bash
Usage: sip_bridge <HOMESERVER_URL> <USERNAME> <PASSWORD> <SIP_SERVER_DOMAIN> <SIP_SERVER_ADDRESS> <SIP_USERNAME> <SIP_PASSWORD>

Arguments:
  <HOMESERVER_URL>      
  <USERNAME>            
  <PASSWORD>            
  <SIP_SERVER_DOMAIN>   
  <SIP_SERVER_ADDRESS>  
  <SIP_USERNAME>        
  <SIP_PASSWORD>        

Options:
  -h, --help     Print help
  -V, --version  Print version
```
