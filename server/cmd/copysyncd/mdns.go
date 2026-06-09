package main

import (
	"context"
	"log/slog"
	"net"
	"strconv"

	"github.com/grandcat/zeroconf"
)

// registerMDNS advertises the server as _copysync._tcp.local so clients can
// discover it without a manually typed IP. Best-effort: a failure is logged, not
// fatal; the registration is torn down when ctx is cancelled.
func registerMDNS(ctx context.Context, name, serverID, httpsAddr string, log *slog.Logger) {
	_, portStr, err := net.SplitHostPort(httpsAddr)
	if err != nil {
		portStr = httpsAddr
	}
	port, err := strconv.Atoi(portStr)
	if err != nil || port == 0 {
		log.Warn("mDNS disabled: cannot parse port", "addr", httpsAddr)
		return
	}
	srv, err := zeroconf.Register(name, "_copysync._tcp", "local.", port,
		[]string{"id=" + serverID, "name=" + name, "proto=1"}, nil)
	if err != nil {
		log.Warn("mDNS registration failed", "err", err)
		return
	}
	log.Info("mDNS advertising", "service", "_copysync._tcp", "port", port)
	go func() {
		<-ctx.Done()
		srv.Shutdown()
	}()
}
