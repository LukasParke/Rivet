package network

import (
	"net"
	"time"
)

// FindSourceIP finds the best source IP address for connecting to a destination IP.
// It looks for an interface with an IP in the same subnet as the destination.
func FindSourceIP(destIP net.IP) net.IP {
	if destIP == nil {
		return nil
	}
	
	interfaces, err := net.Interfaces()
	if err != nil {
		return nil
	}
	
	// If destination is IPv4, prefer IPv4 source; if IPv6, prefer IPv6 source
	isIPv4 := destIP.To4() != nil
	
	for _, iface := range interfaces {
		addrs, err := iface.Addrs()
		if err != nil {
			continue
		}
		for _, addr := range addrs {
			ipnet, ok := addr.(*net.IPNet)
			if !ok {
				continue
			}
			srcIP := ipnet.IP
			
			// Match IP version
			if isIPv4 && srcIP.To4() == nil {
				continue
			}
			if !isIPv4 && srcIP.To4() != nil {
				continue
			}
			
			// Prefer non-loopback, non-link-local addresses
			if srcIP.IsLoopback() || srcIP.IsLinkLocalUnicast() {
				continue
			}
			
			// If destination is in same subnet, use this source IP
			if ipnet.Contains(destIP) {
				return srcIP
			}
			
			// For private IPs, prefer matching first octet (same network)
			if destIP.To4() != nil && srcIP.To4() != nil {
				dest := destIP.To4()
				src := srcIP.To4()
				if dest[0] == src[0] && (dest[0] == 10 || (dest[0] == 172 && dest[1] >= 16 && dest[1] <= 31) || dest[0] == 192) {
					return srcIP
				}
			}
		}
	}
	
	return nil
}

// isSameSubnet checks if the destination IP is on the same subnet as any local interface.
func isSameSubnet(destIP net.IP) bool {
	if destIP == nil {
		return false
	}
	
	interfaces, err := net.Interfaces()
	if err != nil {
		return false
	}
	
	for _, iface := range interfaces {
		addrs, err := iface.Addrs()
		if err != nil {
			continue
		}
		for _, addr := range addrs {
			ipnet, ok := addr.(*net.IPNet)
			if !ok {
				continue
			}
			// Check if destination is in the same subnet
			if ipnet.Contains(destIP) {
				return true
			}
		}
	}
	return false
}

// DialWithSourceIP creates a connection that binds to an appropriate source IP.
// This fixes "no route to host" issues on macOS when curl works but Go's net.Dial doesn't.
// For destinations on the same subnet, we use plain net.Dial (like curl) to avoid routing issues.
func DialWithSourceIP(addr string) (net.Conn, error) {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		return nil, err
	}
	
	// Resolve destination IP
	destIPs, err := net.LookupIP(host)
	if err != nil {
		return nil, err
	}
	
	var destIP net.IP
	// Prefer IPv4
	for _, ip := range destIPs {
		if ip.To4() != nil {
			destIP = ip
			break
		}
	}
	if destIP == nil && len(destIPs) > 0 {
		destIP = destIPs[0]
	}
	
	// For same-subnet destinations, use plain net.DialTimeout (like curl does).
	// This avoids "no route to host" errors on macOS that can occur with custom dialers.
	if destIP != nil && isSameSubnet(destIP) {
		// Force IPv4 for private IP ranges
		network := "tcp"
		if destIP.To4() != nil && (destIP.IsPrivate() || destIP.IsLoopback()) {
			network = "tcp4"
		}
		return net.DialTimeout(network, addr, 5*time.Second)
	}
	
	// For different subnets, use custom dialer with source IP binding
	dialer := &net.Dialer{
		Timeout:   5 * time.Second,
		KeepAlive: 30 * time.Second,
	}
	
	srcIP := FindSourceIP(destIP)
	if srcIP != nil {
		dialer.LocalAddr = &net.TCPAddr{IP: srcIP}
	}
	
	// Force IPv4 for private IP ranges to avoid routing issues
	network := "tcp"
	if destIP != nil && destIP.To4() != nil && (destIP.IsPrivate() || destIP.IsLoopback()) {
		network = "tcp4"
	}
	
	return dialer.Dial(network, addr)
}

