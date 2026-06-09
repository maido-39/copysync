package main

import (
	"context"
	"flag"
	"fmt"
	"strings"
	"time"

	"github.com/syaro/copysync/internal/protocol"
)

// cmdPool sets this device's share pool. Clips with "all" targets only route
// among devices in the same pool, so pools partition who shares with whom.
func cmdPool(args []string) error {
	fs := flag.NewFlagSet("pool", flag.ExitOnError)
	cfgPath := fs.String("config", defaultConfigPath(), "config file path")
	// Accept the pool NAME positional before OR after flags (Go's flag package
	// otherwise stops parsing at the first positional).
	var name string
	var rest []string
	for _, a := range args {
		if name == "" && !strings.HasPrefix(a, "-") {
			name = a
			continue
		}
		rest = append(rest, a)
	}
	_ = fs.Parse(rest)
	if name == "" {
		return fmt.Errorf("usage: copyctl pool NAME [--config FILE]")
	}
	cfg, err := loadConfig(*cfgPath)
	if err != nil {
		return fmt.Errorf("load config (run `copyctl pair` first): %w", err)
	}
	cl, err := newClient(cfg, nil)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	conn, ok, err := cl.connect(ctx)
	if err != nil {
		return err
	}
	defer conn.CloseNow()
	if err := writeMsg(ctx, conn, protocol.TypeSetPool, protocol.SetPool{Pool: name}); err != nil {
		return err
	}
	fmt.Printf("set pool to %q on %q\n", name, ok.ServerName)
	// Give the server a moment to persist before we close the connection.
	rctx, rcancel := context.WithTimeout(ctx, 1500*time.Millisecond)
	defer rcancel()
	_, _ = readMsg(rctx, conn)
	return nil
}
