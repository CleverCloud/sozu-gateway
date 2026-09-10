package runner

import (
	"encoding/json"
	"flag"
	"fmt"
	"net"
	"os"
	"sort"
	"strings"
	"testing"
	"time"

	"github.com/stretchr/testify/require"
	clientset "k8s.io/client-go/kubernetes"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/gateway-api/conformance"
	"sigs.k8s.io/gateway-api/conformance/tests"
)

const upstreamRevision = "ca6c2a65454737236fb7a937bd9b17e42b07e9de"

var (
	selectedTests  = flag.String("selected-tests", "", "Comma-separated exact upstream ShortNames; empty runs all selected profiles")
	catalog        = flag.Bool("catalog", false, "Print the pinned upstream test catalog without contacting Kubernetes")
	probeAddresses = flag.String("probe-addresses", "", "Published Service addresses to check from the runner, without changing Gateway addresses")
)

type catalogEntry struct {
	Name     string   `json:"name"`
	Features []string `json:"features"`
}

func testCatalog() []catalogEntry {
	entries := make([]catalogEntry, 0, len(tests.ConformanceTests))
	for _, test := range tests.ConformanceTests {
		entry := catalogEntry{Name: test.ShortName}
		for _, feature := range test.Features {
			entry.Features = append(entry.Features, string(feature))
		}
		sort.Strings(entry.Features)
		entries = append(entries, entry)
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name < entries[j].Name })
	return entries
}

func selection(raw string, entries []catalogEntry) (map[string]bool, error) {
	selected := map[string]bool{}
	if raw == "" {
		return selected, nil
	}
	known := map[string]bool{}
	for _, entry := range entries {
		known[entry.Name] = true
	}
	for _, name := range strings.Split(raw, ",") {
		if !known[name] {
			return nil, fmt.Errorf("unknown upstream ShortName %q", name)
		}
		if selected[name] {
			return nil, fmt.Errorf("duplicate upstream ShortName %q", name)
		}
		selected[name] = true
	}
	return selected, nil
}

func TestMain(m *testing.M) {
	flag.Parse()
	if *catalog {
		err := json.NewEncoder(os.Stdout).Encode(struct {
			Revision string         `json:"revision"`
			Tests    []catalogEntry `json:"tests"`
		}{upstreamRevision, testCatalog()})
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(2)
		}
		os.Exit(0)
	}
	if _, err := selection(*selectedTests, testCatalog()); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}
	os.Exit(m.Run())
}

func TestSelection(t *testing.T) {
	entries := []catalogEntry{{Name: "HTTPRouteWeight"}, {Name: "UDPRoute"}}
	for _, raw := range []string{"httpRouteWeight", "HTTPRouteWeight,", "UDPRoute,UDPRoute", "DoesNotExist"} {
		_, err := selection(raw, entries)
		require.Error(t, err, raw)
	}
	got, err := selection("UDPRoute,HTTPRouteWeight", entries)
	require.NoError(t, err)
	require.Len(t, got, 2)
	got, err = selection("", entries)
	require.NoError(t, err)
	require.Empty(t, got)
}

func TestGatewayConformance(t *testing.T) {
	selected, err := selection(*selectedTests, testCatalog())
	require.NoError(t, err)
	opts := conformance.DefaultOptions(t)
	require.Empty(t, opts.SkipTests, "select ShortNames through --selected-tests")
	require.Empty(t, opts.RunTest, "select ShortNames through --selected-tests")
	require.True(t, opts.CleanupBaseResources)
	require.True(t, opts.CleanupTestResources)
	if len(selected) != 0 {
		for _, test := range tests.ConformanceTests {
			if selected[test.ShortName] {
				// A requested test must execute rather than disappear behind
				// feature selection. These are run options, not product claims.
				opts.SupportedFeatures = append(opts.SupportedFeatures, test.Features...)
			} else {
				opts.SkipTests = append(opts.SkipTests, test.ShortName)
			}
		}
	}
	opts.RestConfig.QPS = 100
	opts.RestConfig.Burst = 200
	opts.ClientOptions.Scheme = opts.Client.Scheme()
	opts.Client, err = client.New(opts.RestConfig, opts.ClientOptions)
	require.NoError(t, err)
	opts.Clientset, err = clientset.NewForConfig(opts.RestConfig)
	require.NoError(t, err)
	for _, address := range strings.Split(*probeAddresses, ",") {
		if address == "" {
			continue
		}
		conn, err := net.DialTimeout("tcp", net.JoinHostPort(address, "80"), 10*time.Second)
		require.NoError(t, err, "published address must be reachable from the runner: %s", address)
		require.NoError(t, conn.Close())
	}
	t.Logf("Upstream %s; Kubernetes API QPS=100 burst=200", upstreamRevision)
	conformance.RunConformanceWithOptions(t, opts)
}
