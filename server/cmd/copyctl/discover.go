package main

import (
	"context"
	"flag"
	"fmt"
	"time"

	"github.com/grandcat/zeroconf"
)

// cmdDiscover browses the LAN for CopySync servers advertised over mDNS.
func cmdDiscover(args []string) error {
	fs := flag.NewFlagSet("discover", flag.ExitOnError)
	timeout := fs.Duration("timeout", 4*time.Second, "how long to browse")
	_ = fs.Parse(args)

	resolver, err := zeroconf.NewResolver(nil)
	if err != nil {
		return err
	}
	entries := make(chan *zeroconf.ServiceEntry)
	seen := map[string]bool{}
	ctx, cancel := context.WithTimeout(context.Background(), *timeout)
	defer cancel()

	go func() {
		for e := range entries {
			if len(e.AddrIPv4) == 0 {
				continue
			}
			url := fmt.Sprintf("https://%s:%d", e.AddrIPv4[0].String(), e.Port)
			if seen[url] {
				continue
			}
			seen[url] = true
			fmt.Printf("  %-22s %s  %v\n", e.Instance, url, e.Text)
		}
	}()

	fmt.Printf("browsing _copysync._tcp for %s …\n", *timeout)
	if err := resolver.Browse(ctx, "_copysync._tcp", "local.", entries); err != nil {
		return err
	}
	<-ctx.Done()
	if len(seen) == 0 {
		fmt.Println("  (no servers found)")
	}
	return nil
}
