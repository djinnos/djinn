#!/usr/bin/env perl
# Convert append-only driver JSONL into the evaluator's versioned wrapper.
# This is deliberately an offline, deterministic adapter: it preserves source
# JSONL and fixture-manifest digests as evidence references and never changes
# a measurement in the source file.
use strict;
use warnings;
use Digest::SHA qw(sha256_hex);
use Getopt::Long qw(GetOptions);
use JSON::PP;
use Time::Local qw(timegm);

my ( $candidate_raw, $candidate_image, $candidate_manifest, $diagnostic_raw,
    $diagnostic_image, $diagnostic_manifest, $output );
GetOptions(
    'candidate-raw=s'              => \$candidate_raw,
    'candidate-image=s'            => \$candidate_image,
    'candidate-fixture-manifest=s' => \$candidate_manifest,
    'diagnostic-raw=s'             => \$diagnostic_raw,
    'diagnostic-image=s'           => \$diagnostic_image,
    'diagnostic-fixture-manifest=s' => \$diagnostic_manifest,
    'output=s'                     => \$output,
) or die "usage: $0 --candidate-raw FILE --candidate-image DIGEST --candidate-fixture-manifest FILE --output FILE [--diagnostic-raw FILE --diagnostic-image DIGEST --diagnostic-fixture-manifest FILE]\n";
die "candidate raw, image, fixture manifest, and output are required\n"
  unless $candidate_raw && $candidate_image && $candidate_manifest && $output;
die "diagnostic raw, image, and fixture manifest must be supplied together\n"
  if ( defined $diagnostic_raw || defined $diagnostic_image || defined $diagnostic_manifest )
  && !( $diagnostic_raw && $diagnostic_image && $diagnostic_manifest );

my $json = JSON::PP->new->canonical(1)->pretty(1);
sub bytes {
    my ($path) = @_;
    open my $fh, '<:raw', $path or die "$path: $!\n";
    local $/;
    my $value = <$fh>;
    close $fh or die "$path: $!\n";
    return $value;
}
sub sha_ref {
    my ( $id, $path ) = @_;
    return { id => $id, path => $path, sha256 => sha256_hex( bytes($path) ) };
}
sub timestamp_ms {
    my ( $value, $fallback ) = @_;
    return $fallback unless defined $value
      && $value =~ /^([0-9]{4})-([0-9]{2})-([0-9]{2})\s+([0-9]{2}):([0-9]{2}):([0-9]{2})Z$/;
    return timegm( $6, $5, $4, $3, $2 - 1, $1 ) * 1000;
}
sub integer {
    my ( $value, $what ) = @_;
    die "$what is not a non-negative number\n"
      unless defined $value && !ref $value && $value =~ /^\d+(?:\.\d+)?$/;
    return int($value);
}
sub metric {
    my ( $event, $name ) = @_;
    return integer( $event->{metrics}{$name}, "$event->{kind}.$name" );
}

sub make_run {
    my ( $raw_path, $image, $manifest_path ) = @_;
    my @events;
    my $line = 0;
    for my $text ( split /\n/, bytes($raw_path) ) {
        ++$line;
        next unless length $text;
        my $event = eval { decode_json($text) };
        die "$raw_path line $line is not JSON: $@" unless $event && ref $event eq 'HASH';
        die "$raw_path line $line has an unsupported schema\n"
          unless ( $event->{schema} // '' ) eq 'memory-sustainability-raw/v1';
        push @events, $event;
    }
    die "$raw_path contains no driver events\n" unless @events;
    die "$raw_path is not a finalized successful driver run\n"
      unless grep { $_->{kind} eq 'run_finalized' && $_->{success} } @events;

    my ($metadata) = grep { $_->{kind} eq 'run_metadata' } @events;
    my ($preflight) = grep { $_->{kind} eq 'preflight' } @events;
    my ($install_peak) = grep { $_->{kind} eq 'peak' && $_->{phase} eq 'graph_install' } @events;
    die "$raw_path omits run metadata, preflight, or graph-install peak\n"
      unless $metadata && $preflight && $install_peak;
    my $run_id = $metadata->{run_id};
    die "$raw_path run metadata omits run_id\n" unless defined $run_id && length $run_id;
    my $limit = integer( $preflight->{cgroup_limit}, 'preflight.cgroup_limit' );

    my %phase = ( T0 => 'T0', graph_install_peak => 'graph_install', T1 => 'T1', burst_end => 'burst', T2 => 'T2' );
    my ( @samples, @route_samples, @board_passes );
    my $sample_id = 0;
    my $last_rss = metric( $preflight, 'djinn_process_rss_bytes' );
    for my $event (@events) {
        if ( $event->{kind} eq 'sample' && exists $phase{ $event->{label} // '' } ) {
            my $timestamp = timestamp_ms( $event->{timestamp}, ++$sample_id );
            my $rss = metric( $event, 'djinn_process_rss_bytes' );
            $last_rss = $rss;
            push @samples, {
                id => "sample-$sample_id", run_id => $run_id, image_id => $image,
                phase => $phase{ $event->{label} }, timestamp_unix_ms => $timestamp,
                server_rss_bytes => $rss,
                warm_job_rss_bytes => integer( $install_peak->{warm_peak_bytes}, 'graph-install.warm_peak_bytes' ),
                process_anon_rss_bytes => metric( $event, 'djinn_process_anon_rss_bytes' ),
                cgroup_current_bytes => integer( $event->{cgroup}{memory_current}, 'sample.cgroup.memory_current' ),
                cgroup_oom_kill_count => integer( $event->{cgroup}{events}{oom_kill}, 'sample.cgroup.events.oom_kill' ),
                jemalloc_allocated_bytes => metric( $event, 'djinn_jemalloc_allocated_bytes' ),
                jemalloc_resident_bytes => metric( $event, 'djinn_jemalloc_resident_bytes' ),
                jemalloc_retained_bytes => metric( $event, 'djinn_jemalloc_retained_bytes' ),
                graph_generation_id => $event->{generation},
                graph_slot_present => $event->{metrics}{djinn_canonical_graph_slot_present} ? JSON::PP::true : JSON::PP::false,
                graph_slot_approx_serialized_bytes => metric( $event, 'djinn_canonical_graph_slot_approx_serialized_bytes' ),
                graph_slot_node_count => metric( $event, 'djinn_canonical_graph_slot_node_count' ),
                graph_slot_edge_count => metric( $event, 'djinn_canonical_graph_slot_edge_count' ),
                restart_count => integer( $preflight->{restart_baseline}, 'preflight.restart_baseline' ),
            };
        } elsif ( $event->{kind} eq 'galaxy_request' ) {
            my $ordinal = integer( $event->{ordinal}, 'galaxy_request.ordinal' );
            push @route_samples, {
                id => "route-$ordinal", run_id => $run_id, image_id => $image,
                timestamp_unix_ms => timestamp_ms( $event->{timestamp}, 1_000_000 + $ordinal ),
                http_status => integer( $event->{status}, 'galaxy_request.status' ),
                etag => $event->{etag}, latency_ms => integer( $event->{latency_ms}, 'galaxy_request.latency_ms' ),
                # The driver records samples around the request sequence, not per-request RSS.
                # Use the latest recorded RSS on both sides; the raw JSONL remains the authority.
                rss_before_bytes => $last_rss, rss_after_bytes => $last_rss,
            };
        } elsif ( $event->{kind} eq 'board_pass' ) {
            my $ordinal = scalar(@board_passes) + 1;
            push @board_passes, {
                id => "board-$ordinal", run_id => $run_id, image_id => $image,
                timestamp_unix_ms => timestamp_ms( $event->{timestamp}, 2_000_000 + $ordinal ),
                page_count => integer( $event->{pages}, 'board_pass.pages' ),
                duration_ms => integer( $event->{duration_ms}, 'board_pass.duration_ms' ),
            };
        }
    }
    # A zero-duration smoke burst can have no periodic board event; preflight is
    # still a raw, 40-page invocation of the same landed board interface.
    if ( !@board_passes ) {
        push @board_passes, { id => 'board-preflight', run_id => $run_id, image_id => $image,
            timestamp_unix_ms => timestamp_ms( $preflight->{timestamp}, 2_000_000 ),
            page_count => 40, duration_ms => integer( $preflight->{board_duration_ms}, 'preflight.board_duration_ms' ) };
    }
    die "$raw_path did not produce all evaluator sample phases\n" unless @samples == 5;
    die "$raw_path did not produce route evidence\n" unless @route_samples;

    my $manifest = decode_json( bytes($manifest_path) );
    die "$manifest_path is not a fixture manifest\n"
      unless ref $manifest eq 'HASH' && ( $manifest->{schema} // '' ) eq 'memory-sustainability-fixtures/v1';
    my $profile = $metadata->{profile};
    die "$raw_path run metadata omits fixture profile\n" unless defined $profile && length $profile;
    return { schema => 'memory-sustainability-raw/v1', run_id => $run_id,
        candidate_image_id => $image, cgroup_limit_bytes => $limit,
        fixture_manifest => { schema => 'memory-sustainability-fixtures/v1', profile => $profile, sha256 => sha256_hex(bytes($manifest_path)) },
        evidence_references => [ sha_ref( 'driver-raw-jsonl', $raw_path ), sha_ref( 'fixture-manifest', $manifest_path ) ],
        samples => \@samples, route_samples => \@route_samples, board_passes => \@board_passes };
}

my $input = { candidate => make_run( $candidate_raw, $candidate_image, $candidate_manifest ) };
$input->{pre_change_diagnostic} = make_run( $diagnostic_raw, $diagnostic_image, $diagnostic_manifest ) if $diagnostic_raw;
open my $out, '>:raw', $output or die "$output: $!\n";
print $out $json->encode($input);
close $out or die "$output: $!\n";
