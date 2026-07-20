#!/usr/bin/env perl
# Cluster-independent integration smoke for the operational protocol.
use strict;
use warnings;
use Getopt::Long qw(GetOptions);
use JSON::PP;
use Digest::SHA qw(sha256_hex);
use File::Path qw(make_path);

my $here = __FILE__;
$here =~ s{/[^/]+$}{};
my $root = $here;
$root =~ s{/server/docs/operational/memory-sustainability$}{};
$root = '.' if $root eq '';
my $out;
GetOptions('output-dir=s' => \$out) or die "usage: perl smoke.pl --output-dir DIR\n";
die "usage: perl smoke.pl --output-dir DIR\n" unless $out;
make_path($out);
my $fixture_dir = "$out/fixtures";
my $driver_raw = "$out/driver-raw.jsonl";
my $eval_input = "$out/evaluator-input.json";
my $eval_json = "$out/evaluation.json";
my $eval_md = "$out/evaluation.md";

sub run {
    system @_;
    die "command failed (exit " . ($? >> 8) . ")\n" if $?;
}
sub bytes {
    my ($path) = @_;
    open my $fh, '<:raw', $path or die "$path: $!\n";
    local $/;
    my $v = <$fh>;
    close $fh;
    return $v;
}

run('perl', "$here/fixtures/generate.pl", '--profile', 'smoke', '--output-dir', $fixture_dir, '--validate');
run('perl', "$here/driver/memory_workload.pl", '--fake', '--profile', 'smoke',
    '--output', $driver_raw, '--t0-seconds', '0', '--t1-seconds', '0',
    '--burst-seconds', '0', '--t2-seconds', '0', '--request-count', '6');

my $j = JSON::PP->new->canonical(1)->pretty(1);
my @events = map { decode_json($_) } grep { length } split /\n/, bytes($driver_raw);
my ($meta) = grep { $_->{kind} eq 'run_metadata' } @events;
my ($preflight) = grep { $_->{kind} eq 'preflight' } @events;
my ($peak) = grep { $_->{kind} eq 'peak' && $_->{phase} eq 'graph_install' } @events;
die "fake collection omitted required evidence\n" unless $meta && $preflight && $peak;
my $run_id = $meta->{run_id};
my $image_id = 'synthetic/fake-driver';
my %phase = (T0 => 'T0', graph_install_peak => 'graph_install', T1 => 'T1', burst_end => 'burst', T2 => 'T2');
my $n = 0;
my @samples;
for my $e (grep { $_->{kind} eq 'sample' && exists $phase{$_->{label}} } @events) {
    my $m = $e->{metrics};
    push @samples, {
        id => 'sample-' . ++$n, run_id => $run_id, image_id => $image_id,
        phase => $phase{$e->{label}}, timestamp_unix_ms => $n,
        server_rss_bytes => 0 + $m->{djinn_process_rss_bytes},
        warm_job_rss_bytes => 0 + $peak->{warm_peak_bytes},
        process_anon_rss_bytes => 0 + $m->{djinn_process_anon_rss_bytes},
        cgroup_current_bytes => 0 + $e->{cgroup}{memory_current},
        cgroup_oom_kill_count => 0 + $e->{cgroup}{events}{oom_kill},
        jemalloc_allocated_bytes => 0 + $m->{djinn_jemalloc_allocated_bytes},
        jemalloc_resident_bytes => 0 + $m->{djinn_jemalloc_resident_bytes},
        jemalloc_retained_bytes => 0 + $m->{djinn_jemalloc_retained_bytes},
        graph_generation_id => $e->{generation},
        graph_slot_present => $m->{djinn_canonical_graph_slot_present} ? JSON::PP::true : JSON::PP::false,
        graph_slot_approx_serialized_bytes => 0 + $m->{djinn_canonical_graph_slot_approx_serialized_bytes},
        graph_slot_node_count => 0 + $m->{djinn_canonical_graph_slot_node_count},
        graph_slot_edge_count => 0 + $m->{djinn_canonical_graph_slot_edge_count},
        restart_count => 0 + $preflight->{restart_baseline},
    };
}
die "fake collection did not produce every evaluator phase\n" unless @samples == 5;
my @routes;
for my $e (grep { $_->{kind} eq 'galaxy_request' } @events) {
    push @routes, { id => "route-$e->{ordinal}", run_id => $run_id, image_id => $image_id,
        timestamp_unix_ms => 100 + $e->{ordinal}, http_status => 0 + $e->{status}, etag => $e->{etag},
        latency_ms => int($e->{latency_ms}), rss_before_bytes => 1_000_000_000,
        rss_after_bytes => 1_000_000_000 };
}
my @boards;
my $board_n = 0;
for my $e (grep { $_->{kind} eq 'board_pass' } @events) {
    push @boards, { id => 'board-' . ++$board_n, run_id => $run_id, image_id => $image_id,
        timestamp_unix_ms => 200 + $board_n, page_count => 0 + $e->{pages},
        duration_ms => int($e->{duration_ms}) };
}
# A zero-duration burst may have no periodic pass; preflight proves the same 40-page interface.
@boards = ({ id => 'board-preflight', run_id => $run_id, image_id => $image_id,
    timestamp_unix_ms => 200, page_count => 0 + $preflight->{board_duration_ms} * 0 + 40,
    duration_ms => int($preflight->{board_duration_ms}) }) unless @boards;
my $manifest = "$here/fixtures/manifest.json";
my $input = { candidate => { schema => 'memory-sustainability-raw/v1', run_id => $run_id,
    candidate_image_id => $image_id, cgroup_limit_bytes => 0 + $preflight->{cgroup_limit},
    fixture_manifest => { schema => 'memory-sustainability-fixtures/v1', profile => 'smoke', sha256 => sha256_hex(bytes($manifest)) },
    evidence_references => [ { id => 'driver-raw', path => $driver_raw, sha256 => sha256_hex(bytes($driver_raw)) } ],
    samples => \@samples, route_samples => \@routes, board_passes => \@boards } };
open my $fh, '>:raw', $eval_input or die "$eval_input: $!\n";
print $fh $j->encode($input);
close $fh;
run('perl', "$here/evaluator/evaluate.pl", '--input', $eval_input, '--json-out', $eval_json, '--report-out', $eval_md);
print "PASS: fake collection and evaluation completed\nraw=$driver_raw\ninput=$eval_input\nresult=$eval_json\nreport=$eval_md\n";
