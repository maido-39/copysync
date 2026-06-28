package httpapi

import (
	"net"
	"net/http"
	"sync"
	"time"

	"golang.org/x/time/rate"
)

const (
	// limiterTTL is how long an idle per-IP limiter is kept before the sweeper
	// evicts it. It is comfortably longer than any rate window (currently 2s /
	// burst 5) so an actively-rate-limited client is never evicted mid-flight.
	limiterTTL = 10 * time.Minute
	// limiterSweepInterval is how often idle limiters are reclaimed.
	limiterSweepInterval = time.Minute
)

type limiterEntry struct {
	lim      *rate.Limiter
	lastSeen time.Time
}

// ipLimiter holds a token-bucket rate limiter per client IP. Idle entries are
// reclaimed by a background sweeper so the map size stays proportional to the
// number of active clients rather than the lifetime count of distinct (and
// possibly attacker-controlled) source IPs.
type ipLimiter struct {
	mu       sync.Mutex
	limiters map[string]*limiterEntry
	r        rate.Limit
	burst    int
	now      func() time.Time
	stop     chan struct{}
	stopOnce sync.Once
}

func newIPLimiter(r rate.Limit, burst int) *ipLimiter {
	l := &ipLimiter{
		limiters: make(map[string]*limiterEntry),
		r:        r,
		burst:    burst,
		now:      time.Now,
		stop:     make(chan struct{}),
	}
	go l.sweepLoop()
	return l
}

func (l *ipLimiter) get(ip string) *rate.Limiter {
	l.mu.Lock()
	defer l.mu.Unlock()
	now := l.now()
	e, ok := l.limiters[ip]
	if !ok {
		e = &limiterEntry{lim: rate.NewLimiter(l.r, l.burst)}
		l.limiters[ip] = e
	}
	e.lastSeen = now
	return e.lim
}

func (l *ipLimiter) allow(r *http.Request) bool {
	return l.get(clientIP(r)).Allow()
}

// sweepLoop periodically evicts limiters idle longer than limiterTTL.
func (l *ipLimiter) sweepLoop() {
	ticker := time.NewTicker(limiterSweepInterval)
	defer ticker.Stop()
	for {
		select {
		case <-l.stop:
			return
		case <-ticker.C:
			l.sweep()
		}
	}
}

func (l *ipLimiter) sweep() {
	l.mu.Lock()
	defer l.mu.Unlock()
	cutoff := l.now().Add(-limiterTTL)
	for ip, e := range l.limiters {
		if e.lastSeen.Before(cutoff) {
			delete(l.limiters, ip)
		}
	}
}

// Stop halts the background sweeper. Safe to call more than once.
func (l *ipLimiter) Stop() {
	l.stopOnce.Do(func() { close(l.stop) })
}

func clientIP(r *http.Request) string {
	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err != nil {
		return r.RemoteAddr
	}
	return host
}
