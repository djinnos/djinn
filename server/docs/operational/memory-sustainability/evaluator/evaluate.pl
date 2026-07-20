#!/usr/bin/env perl
# Offline evaluator for the memory-sustainability release gate.
# Consumes a versioned append-only raw evidence contract and emits a
# deterministic machine-readable JSON result plus a human-readable Markdown
# rendering generated from the same result.
use strict;
use warnings;
use JSON::PP;
use Digest::SHA qw(sha256_hex);
use Getopt::Long qw(GetOptions);
use B qw(svref_2object SVf_IOK SVf_POK);

my $M = 1024 * 1024;
my $RAW = 'memory-sustainability-raw/v1';
my $REP = 'memory-sustainability-report/v1';

my ( $in, $jo, $mo );
GetOptions( 'input=s' => \$in, 'json-out=s' => \$jo, 'report-out=s' => \$mo )
  or die "usage\n";
die "usage\n" unless $in && $jo && $mo;

my $J = JSON::PP->new->canonical->pretty;

sub enc { $J->encode( $_[0] ) }
sub h { ref( $_[0] ) eq 'HASH' }
sub a { ref( $_[0] ) eq 'ARRAY' }
sub out {
    open my $f, '>:raw', $_[0] or die $!;
    print $f $_[1];
    close $f;
}

# Strict JSON non-negative integer check: must be a real JSON integer (IVOK),
# not a digit string or float. Uses B::svref_2object flags to distinguish.
# SVf_POK rejection is critical: JSON::PP may dual-vivify strings to integers,
# so we must reject values that also carry the string flag.
sub num {
    my ( $v, $p, $e ) = @_;
    my $f = defined $v && !ref $v ? svref_2object( \$v )->FLAGS : 0;
    if ( !$f || !( $f & SVf_IOK ) || ( $f & SVf_POK ) || $v < 0 ) {
        push @$e, "$p must be a JSON non-negative integer";
        return;
    }
    return 0 + $v;
}

# Strict JSON non-negative integer check that also returns the value.
sub str {
    my ( $v, $p, $e ) = @_;
    push @$e, "$p must be a non-empty string"
      unless defined $v && !ref $v && length $v;
    return $v;
}

# Build a check result: observed, threshold, units, evidence refs, pass/fail.
sub chk {
    my ( $name, $observed, $threshold, $units, $ev, $ok, $errors ) = @_;
    my $x = {
        name      => $name,
        observed  => $observed,
        threshold => $threshold,
        units     => $units,
        evidence  => $ev,
        status    => @$errors ? 'error' : ( $ok ? 'pass' : 'fail' ),
    };
    $x->{errors} = [@$errors] if @$errors;
    return $x;
}

sub evalrun {
    my $r = shift;
    my @e;

    # Wrapper must be a JSON object.
    return [ chk( 'input_contract', undef, 'raw object', 'n/a', [], 0,
        ['raw must be an object'] ) ]
      unless h $r;

    # Schema version check (exact match required; forward versions rejected).
    push @e, "unsupported raw schema; expected $RAW"
      unless defined $r->{schema} && !ref $r->{schema} && $r->{schema} eq $RAW;

    my ( $run, $image ) = @{ $r }{qw(run_id candidate_image_id)};
    str( $run, 'run_id', \@e );
    str( $image, 'candidate_image_id', \@e );

    my $l = num( $r->{cgroup_limit_bytes}, 'cgroup_limit_bytes', \@e );
    push @e, 'cgroup_limit_bytes must be exactly 4294967296 (4 GiB)'
      unless defined $l && $l == 4096 * $M;

    # --- Samples: append-only timestamped stream ---
    my $s = $r->{samples};
    if ( !a $s ) {
        push @e, 'samples must be an array';
        $s = [];
    }

    my ( %phase, %id );
    my @v;    # validated samples: [index, sample]

    my @fields = qw(
      timestamp_unix_ms server_rss_bytes warm_job_rss_bytes
      process_anon_rss_bytes cgroup_current_bytes cgroup_oom_kill_count
      jemalloc_allocated_bytes jemalloc_resident_bytes jemalloc_retained_bytes
      graph_slot_approx_serialized_bytes graph_slot_node_count
      graph_slot_edge_count restart_count
    );

    for my $i ( 0 .. $#$s ) {
        my $x = $s->[$i];
        my $p = "samples[$i]";

        if ( !h $x ) {
            push @e, "$p must be an object";
            next;
        }

        for my $k (qw(id run_id image_id)) {
            str( $x->{$k}, "$p.$k", \@e );
        }

        # Identity must match the run; mixed identities are an error.
        push @e, "$p identity differs from run"
          if defined $x->{run_id}
          && !ref $x->{run_id}
          && $x->{run_id} ne $run
          || defined $x->{image_id}
          && !ref $x->{image_id}
          && $x->{image_id} ne $image;

        # Duplicate evidence IDs are not allowed in the append-only stream.
        push @e, "duplicate evidence id $x->{id}"
          if defined $x->{id} && !ref $x->{id} && $id{ $x->{id} }++;

        my $ph = $x->{phase};
        if ( !defined $ph || ref $ph
          || !grep { $ph eq $_ } qw(T0 graph_install T1 burst T2) )
        {
            push @e, "$p.phase is invalid";
            next;
        }

        push @{ $phase{$ph} }, [ $i, $x ];

        # Strict numeric validation of every measurement field.
        num( $x->{$_}, "$p.$_", \@e ) for @fields;

        # --- Phase-specific graph presence requirements ---
        if ( $ph eq 'T0' ) {
            # T0 is recorded with NO graph installed.
            # graph_generation_id MUST be null (JSON null, not absent/empty).
            if ( !exists $x->{graph_generation_id} ) {
                push @e, "$p.graph_generation_id is required at T0";
            } elsif ( defined $x->{graph_generation_id} ) {
                push @e, "$p.graph_generation_id must be null at T0";
            }
            # graph_slot_present MUST be a JSON boolean and exactly false.
            # Missing, null, string, or number are all errors — not just
            # the case where it is a boolean true.
            if ( !JSON::PP::is_bool( $x->{graph_slot_present} ) ) {
                push @e, "$p.graph_slot_present must be a JSON boolean at T0";
            } elsif ( $x->{graph_slot_present} ) {
                push @e, "$p.graph_slot_present must be false at T0";
            }
        } else {
            # After T0: graph must be present and generation non-empty.
            str( $x->{graph_generation_id}, "$p.graph_generation_id", \@e );
            if ( !JSON::PP::is_bool( $x->{graph_slot_present} ) ) {
                push @e, "$p.graph_slot_present must be a JSON boolean";
            } elsif ( !$x->{graph_slot_present} ) {
                push @e, "$p.graph_slot_present must be true after T0";
            }
        }

        push @v, [ $i, $x ];
    }

    # Required phases: T0, graph_install, T1, T2 must each have exactly one
    # anchor sample. burst may have many samples (or one).
    for my $p (qw(T0 graph_install T1 T2)) {
        if ( !$phase{$p} ) {
            push @e, "missing required phase $p";
        } elsif ( @{ $phase{$p} } != 1 ) {
            push @e, "ambiguous phase anchor $p";
        }
    }
    push @e, 'missing required phase burst' unless $phase{burst};

    # Timestamps must be append-order monotonic.
    for my $i ( 1 .. $#v ) {
        push @e, 'sample timestamps are not append-order monotonic'
          if $v[$i][1]{timestamp_unix_ms} < $v[ $i - 1 ][1]{timestamp_unix_ms};
    }

    # --- Route evidence: append-only stream ---
    my $rt = $r->{route_samples};
    $rt = [] unless a $rt;
    push @e, 'route_samples must be a non-empty array' unless @$rt;

    my @d;    # route RSS deltas
    for my $i ( 0 .. $#$rt ) {
        my $x = $rt->[$i];
        my $p = "route_samples[$i]";

        if ( !h $x ) {
            push @e, "$p must be an object";
            next;
        }

        for my $k (qw(id run_id image_id etag)) {
            str( $x->{$k}, "$p.$k", \@e );
        }

        num( $x->{$_}, "$p.$_", \@e )
          for qw(timestamp_unix_ms http_status latency_ms
            rss_before_bytes rss_after_bytes);

        # http_status must be a JSON integer 200 or 304.
        my $hs   = $x->{http_status};
        my $hsok =
             defined $hs
          && !ref $hs
          && svref_2object( \$hs )->FLAGS & SVf_IOK
          && !( svref_2object( \$hs )->FLAGS & SVf_POK )
          && ( $hs == 200 || $hs == 304 );
        push @e, "$p.http_status must be JSON integer 200 or 304" unless $hsok;

        # Identity must match the run.
        push @e, "$p identity differs from run"
          if defined $x->{run_id}
          && !ref $x->{run_id}
          && $x->{run_id} ne $run
          || defined $x->{image_id}
          && !ref $x->{image_id}
          && $x->{image_id} ne $image;

        push @d, $x->{rss_after_bytes} - $x->{rss_before_bytes}
          if defined $x->{rss_after_bytes}
          && defined $x->{rss_before_bytes}
          && !( svref_2object( \$x->{rss_after_bytes} )->FLAGS & SVf_POK )
          && !( svref_2object( \$x->{rss_before_bytes} )->FLAGS & SVf_POK );
    }

    # --- Board evidence: append-only stream ---
    my $bd = $r->{board_passes};
    $bd = [] unless a $bd;
    push @e, 'board_passes must be a non-empty array' unless @$bd;

    my @b;    # board pass durations
    for my $i ( 0 .. $#$bd ) {
        my $x = $bd->[$i];
        my $p = "board_passes[$i]";

        if ( !h $x ) {
            push @e, "$p must be an object";
            next;
        }

        for my $k (qw(id run_id image_id)) {
            str( $x->{$k}, "$p.$k", \@e );
        }

        num( $x->{$_}, "$p.$_", \@e )
          for qw(timestamp_unix_ms page_count duration_ms);

        push @e, "$p identity differs from run"
          if defined $x->{run_id}
          && !ref $x->{run_id}
          && $x->{run_id} ne $run
          || defined $x->{image_id}
          && !ref $x->{image_id}
          && $x->{image_id} ne $image;

        push @b, $x->{duration_ms} if defined $x->{duration_ms};
    }

    # --- Anchor selection and gate computation ---
    my $an = sub {
        my $p = shift;
        $phase{$p} && @{ $phase{$p} } == 1 ? $phase{$p}[0] : undef;
    };
    my ( $t0, $t1, $t2 ) = map { $an->($_) } qw(T0 T1 T2);

    # Generation stability: check from installed samples (post-T0) through T2.
    # T0 is excluded because no graph is installed at T0.
    my @g = map { $_->[1]{graph_generation_id} }
      grep { $_->[1]{phase} ne 'T0' } @v;
    my $gen_ok = @g
      && !grep { !defined $_ || ref $_ || $_ ne $g[0] } @g;

    # Peak derivation from every sample in the stream.
    my $max = sub {
        my $k  = shift;
        my @x = map { $_->[1]{$k} } @v;
        @x = grep defined, @x;
        return @x ? ( sort { $b <=> $a } @x )[0] : undef;
    };
    my ( $sp, $wp ) =
      ( $max->('server_rss_bytes'), $max->('warm_job_rss_bytes') );

    my $rp = @d ? ( sort { $b <=> $a } @d )[0] : undef;
    my $bp = @b ? ( sort { $b <=> $a } @b )[0] : undef;

    # T2 retention gates (require single-anchor T1 and T2).
    my ( $rd, $rl, $jd );
    if ( $t1 && $t2 ) {
        $rd = $t2->[1]{server_rss_bytes} - $t1->[1]{server_rss_bytes};
        $rl = int( $t1->[1]{server_rss_bytes} / 10 );
        $rl = 128 * $M if $rl < 128 * $M;
        $jd = $t2->[1]{jemalloc_retained_bytes} - $t1->[1]{jemalloc_retained_bytes};
    }

    # OOM/restart monotonic check across all samples.
    my @ce;
    for my $k (qw(cgroup_oom_kill_count restart_count)) {
        for my $i ( 1 .. $#v ) {
            push @ce, "$k is not monotonic"
              if $v[$i][1]{$k} < $v[ $i - 1 ][1]{$k};
        }
    }

    my $ok = !@e;

    # Evidence references use actual array indices (valid JSON pointers).
    my @sev = map { "/samples/$_->[0]/server_rss_bytes" } @v;
    my @wev = map { "/samples/$_->[0]/warm_job_rss_bytes" } @v;
    my @pev = map { "/samples/$_->[0]" } @v;
    my @rev = map { "/route_samples/$_" } 0 .. $#$rt;
    my @bev = map { "/board_passes/$_/duration_ms" } 0 .. $#$bd;

    my $od =
      $t0 && $t2
      ? $t2->[1]{cgroup_oom_kill_count} - $t0->[1]{cgroup_oom_kill_count}
      : undef;
    my $xd =
      $t0 && $t2 ? $t2->[1]{restart_count} - $t0->[1]{restart_count} : undef;

    return [
        chk( 'server_peak',
            $sp, 3584 * $M, 'bytes', \@sev,
            $ok && defined $sp && $sp <= 3584 * $M, \@e ),
        chk( 'warm_job_peak',
            $wp, 3584 * $M, 'bytes', \@wev,
            $ok && defined $wp && $wp <= 3584 * $M, \@e ),
        chk( 'route_rss_delta',
            $rp, 32 * $M, 'bytes', \@rev,
            $ok && defined $rp && $rp <= 32 * $M, \@e ),
        chk( 'board_pass_duration',
            $bp, 120000, 'milliseconds', \@bev,
            $ok && defined $bp && $bp <= 120000, \@e ),
        chk( 'oom_delta',
            $od, 0, 'events', \@pev,
            $ok && !@ce && defined $od && $od == 0, [ @e, @ce ] ),
        chk( 'restart_delta',
            $xd, 0, 'events', \@pev,
            $ok && !@ce && defined $xd && $xd == 0, [ @e, @ce ] ),
        chk( 'same_graph_generation',
            \@g, 'one unchanged installed generation', 'identity', \@pev,
            $ok && $gen_ok, \@e ),
        chk( 't2_rss_delta',
            $rd, $rl, 'bytes',
            [ map { "/samples/$_->[0]/server_rss_bytes" } grep defined, ( $t1, $t2 ) ],
            $ok && defined $rd && $rd <= $rl, \@e ),
        chk( 't2_jemalloc_retained_delta',
            $jd, 256 * $M, 'bytes',
            [ map { "/samples/$_->[0]/jemalloc_retained_bytes" } grep defined, ( $t1, $t2 ) ],
            $ok && defined $jd && $jd <= 256 * $M, \@e ),
    ];
}

# --- Markdown rendering ---
sub tab {
    my $c = shift;
    my @x = (
        '| Check | Status | Observed | Threshold | Units | Evidence |',
        '|---|---|---:|---:|---|---|'
    );
    for (@$c) {
        my $obs =
          !defined $_->{observed} ? 'null'
          : ref $_->{observed}    ? enc( $_->{observed} )
          :                         $_->{observed};
        my $thr = defined $_->{threshold} ? $_->{threshold} : 'null';
        push @x,
          "| $_->{name} | $_->{status} | $obs | $thr | $_->{units} | "
          . join( ', ', @{ $_->{evidence} } ) . ' |';
    }
    return @x;
}

sub render {
    my $r = shift;
    my @x = (
        '# Memory-sustainability evaluation',
        '',
        "Candidate release status: **" . uc( $r->{candidate}{status} ) . '**',
        '',
        tab( $r->{candidate}{checks} ),
    );
    if ( $r->{pre_change_diagnostic} ) {
        push @x, '', '## Pre-change-image diagnostic (non-release-gating)',
          '',
          "Diagnostic status: **"
          . uc( $r->{pre_change_diagnostic}{status} )
          . '**. This section cannot change the candidate release status.',
          '', tab( $r->{pre_change_diagnostic}{checks} );
    }
    return join( "\n", @x ) . "\n";
}

# --- Main ---
my ( $doc, $err );
eval {
    $doc = do {
        open my $f, '<:raw', $in or die $!;
        local $/;
        decode_json(<$f>);
    };
} or $err = $@;

# A valid JSON array (or any non-object) at wrapper root is a contract error,
# not a crash. Coerce non-object roots to undef so evalrun reports an error.
$doc = {} unless h $doc;

my $c = evalrun( $err ? undef : $doc->{candidate} );
my $pass = !grep { $_->{status} ne 'pass' } @$c;
my $result = {
    schema => $REP,
    candidate => {
        status           => $pass ? 'pass' : 'fail',
        checks           => $c,
        raw_measurements => $doc->{candidate},
    },
    raw_input_sha256 => sha256_hex( enc($doc) ),
};

if ( exists $doc->{pre_change_diagnostic} ) {
    my $d = evalrun( $doc->{pre_change_diagnostic} );
    $result->{pre_change_diagnostic} = {
        label => 'pre-change-image diagnostic; not release gating',
        status => ( !grep { $_->{status} ne 'pass' } @$d ) ? 'pass' : 'fail',
        checks => $d,
        raw_measurements => $doc->{pre_change_diagnostic},
    };
}

$result->{human_report} = render($result);

out( $jo, enc($result) );
out( $mo, $result->{human_report} );
exit( $pass ? 0 : 1 );
