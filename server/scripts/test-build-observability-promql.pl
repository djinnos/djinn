#!/usr/bin/env perl
# Evaluate the controlled build-observability PromQL fixture without Prometheus.
use v5.36;
use JSON::PP qw(decode_json);
use FindBin qw($Bin);

my $root = "$Bin/..";
my $doc_path = "$root/docs/operational/build-observability-queries.md";
my $fixture_path = "$root/docs/operational/fixtures/build-observability-promql.json";

sub slurp ($path) {
    open my $fh, '<:encoding(UTF-8)', $path or die "open $path: $!\n";
    local $/;
    return <$fh>;
}

sub assert_true ($condition, $message) {
    die "assertion failed: $message\n" unless $condition;
}

sub assert_close ($actual, $expected, $name) {
    assert_true(defined $actual, "$name: expected a value, got no series");
    assert_true(abs($actual - $expected) <= 1e-12, "$name: got $actual, expected $expected");
}

sub histogram_quantile ($quantile, $buckets) {
    my @ordered = sort {
        $a->[0] eq '+Inf' ? 1 : $b->[0] eq '+Inf' ? -1 : $a->[0] <=> $b->[0]
    } @$buckets;
    my $total = $ordered[-1][1];
    return undef if $total == 0;
    my $rank = $quantile * $total;
    my ($previous_le, $previous_count) = (0, 0);
    for my $bucket (@ordered) {
        my ($le, $count) = @$bucket;
        if ($rank <= $count) {
            return $previous_le if $le eq '+Inf';
            return $le if $count == $previous_count;
            return $previous_le + ($le - $previous_le) * ($rank - $previous_count) / ($count - $previous_count);
        }
        $previous_le = $le unless $le eq '+Inf';
        $previous_count = $count;
    }
    die "histogram is missing a +Inf bucket\n";
}

sub aggregate_bucket_rates ($series, $window_seconds) {
    my %totals;
    for my $sample (@$series) {
        for my $bucket (@{$sample->{buckets}}) {
            $totals{$bucket->{le}} += ($bucket->{end} - $bucket->{start}) / $window_seconds;
        }
    }
    return [map { [$_, $totals{$_}] } keys %totals];
}

sub provider_wait_share ($counters, $window_seconds) {
    my (%rates, %roles);
    for my $sample (@$counters) {
        my $labels = $sample->{labels};
        my ($role, $phase) = @{$labels}{qw(role phase)};
        $roles{$role} = 1;
        $rates{"$role\0$phase"} += ($sample->{end} - $sample->{start}) / $window_seconds;
    }
    my %shares;
    for my $role (keys %roles) {
        my $provider = $rates{"$role\0provider_wait"} // 0;
        my $tool = $rates{"$role\0tool_execution"} // 0;
        my $denominator = $provider + $tool;
        $shares{$role} = $provider / $denominator if $denominator != 0;
    }
    return \%shares;
}

my $document = slurp($doc_path);
for my $required (
    'djinn_cargo_invocation_seconds_bucket{kind="check"}[15m]',
    'djinn_build_slot_queue_wait_seconds_bucket{outcome="admitted"}[15m]',
    'djinn_agent_session_phase_seconds_total{phase="provider_wait"}[15m]',
    'djinn_agent_session_phase_seconds_total{phase="tool_execution"}[15m]',
    'sum by (role)',
) {
    assert_true(index($document, $required) >= 0, "documented query missing $required");
}
my $quantile_count = () = $document =~ /histogram_quantile\(/g;
assert_true($quantile_count >= 3, 'document needs three histogram quantiles');
my @promql_blocks = $document =~ /```promql\n(.*?)```/sg;
my $queries = join "\n", @promql_blocks;
assert_true(index($queries, 'or vector(0)') < 0, 'provider share must not coerce zero with or vector(0)');
assert_true(index($queries, 'clamp_min') < 0, 'provider share must not clamp the denominator');
assert_true(index($document, 'verify_cache_hit_total') < 0, 'document must not mention verify_cache_hit_total');

my $fixture = decode_json(slurp($fixture_path));
my $window = $fixture->{window_seconds};
my $expected = $fixture->{expected};
my $cargo = aggregate_bucket_rates($fixture->{histograms}{cargo_check}, $window);
assert_close(histogram_quantile(0.50, $cargo), $expected->{cargo_check_p50_seconds}, 'cargo p50');
assert_close(histogram_quantile(0.95, $cargo), $expected->{cargo_check_p95_seconds}, 'cargo p95');
my $queue = aggregate_bucket_rates($fixture->{histograms}{admitted_queue_wait}, $window);
assert_close(histogram_quantile(0.95, $queue), $expected->{admitted_queue_wait_p95_seconds}, 'admitted queue-wait p95');
my $shares = provider_wait_share($fixture->{phase_counters}, $window);
for my $role (keys %{$expected->{provider_wait_share_by_role}}) {
    assert_close($shares->{$role}, $expected->{provider_wait_share_by_role}{$role}, "provider share for $role");
}
for my $role (@{$expected->{provider_wait_share_absent_roles}}) {
    assert_true(!exists $shares->{$role}, "provider share for $role must have no series when denominator is zero");
}

say 'build-observability PromQL fixture checks passed';
