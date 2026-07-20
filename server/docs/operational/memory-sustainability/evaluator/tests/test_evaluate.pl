use strict;
use warnings;
use Test::More;
use JSON::PP;
use File::Temp qw(tempdir);

my $S = 'server/docs/operational/memory-sustainability/evaluator/evaluate.pl';
my $M = 1024 * 1024;

# ---------------------------------------------------------------------------
# Fixture builder: produces a baseline passing raw run with values set to the
# exact boundary of every numeric gate, plus append-only samples.
# ---------------------------------------------------------------------------
sub raw {
    my $r   = 'run';
    my $img = 'candidate';
    my $n   = 0;
    my $sample = sub {
        my ( $phase, $rss, $ret ) = @_;
        $rss //= 1024 * $M;
        {
            id                                 => 's' . ++$n,
            run_id                             => $r,
            image_id                           => $img,
            phase                              => $phase,
            timestamp_unix_ms                  => $n,
            server_rss_bytes                   => $rss,
            warm_job_rss_bytes                 => 1024 * $M,
            process_anon_rss_bytes             => 512 * $M,
            cgroup_current_bytes               => 1024 * $M,
            cgroup_oom_kill_count              => 0,
            jemalloc_allocated_bytes           => 1,
            jemalloc_resident_bytes            => 2,
            jemalloc_retained_bytes            => $ret // 512 * $M,
            graph_generation_id                => ( $phase eq 'T0' ? undef : 'g' ),
            graph_slot_present                 => ( $phase eq 'T0' ? JSON::PP::false : JSON::PP::true ),
            graph_slot_approx_serialized_bytes => 1,
            graph_slot_node_count              => 1,
            graph_slot_edge_count              => 1,
            restart_count                      => 0,
        };
    };
    return {
        schema             => 'memory-sustainability-raw/v1',
        run_id             => $r,
        candidate_image_id => $img,
        cgroup_limit_bytes => 4096 * $M,
        samples            => [
            $sample->('T0'),
            $sample->('graph_install', 3584 * $M),
            $sample->('T1'),
            $sample->('burst', 2000 * $M),
            $sample->('burst', 3000 * $M),
            $sample->('T2', 1152 * $M, 768 * $M),
        ],
        route_samples => [
            {
                id                => 'r',
                run_id            => $r,
                image_id          => $img,
                timestamp_unix_ms => 1,
                http_status       => 200,
                etag              => 'x',
                latency_ms        => 1,
                rss_before_bytes  => 1,
                rss_after_bytes   => 32 * $M + 1,
            },
        ],
        board_passes => [
            {
                id                => 'b',
                run_id            => $r,
                image_id          => $img,
                timestamp_unix_ms => 1,
                page_count        => 40,
                duration_ms       => 120000,
            },
        ],
    };
}

# Run the evaluator on a candidate (and optional diagnostic) and return
# (json_result, markdown, exit_code).
sub run_eval {
    my ( $c, $d ) = @_;
    my $z = tempdir( CLEANUP => 1 );
    my $p = "$z/i";
    open my $f, '>', $p or die $!;
    print $f JSON::PP->new->canonical->encode(
        { candidate => $c, ( defined $d ? ( pre_change_diagnostic => $d ) : () ) }
    );
    close $f;
    my $rc = system( 'perl', $S, '--input', $p,
        '--json-out', "$z/o", '--report-out', "$z/m" );
    open $f, '<', "$z/o";
    local $/;
    my $x = decode_json(<$f>);
    close $f;
    open $f, '<', "$z/m";
    my $m = <$f>;
    close $f;
    return ( $x, $m, $rc );
}

sub enc { JSON::PP->new->canonical->encode( $_[0] ) }

# ===================================================================
# 1. Baseline: append-only stream passes with exact boundaries
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;    # route delta exactly 32 MiB
    my ( $a, $m, $rc ) = run_eval($x);
    is( $a->{candidate}{status}, 'pass',
        'append-only stream and exact boundaries pass' );
    is( $rc, 0, 'passing evaluation exits 0' );
    is_deeply( $a->{candidate}{raw_measurements}, $x, 'raw retained' );
}

# ===================================================================
# 2. Deterministic rendering: repeated runs produce byte-identical output
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    my ( $a1, $m1, $rc1 ) = run_eval($x);
    my ( $a2, $m2, $rc2 ) = run_eval($x);
    is( enc($a1), enc($a2), 'JSON rendering deterministic across runs' );
    is( $m1, $m2, 'Markdown rendering deterministic across runs' );
    is( $rc1, $rc2, 'exit code deterministic' );
    # The human_report field in JSON must equal the Markdown file byte-for-byte.
    is( $a1->{human_report}, $m1,
        'human_report field equals Markdown file byte-for-byte' );
}

# ===================================================================
# 3. T0 graph_slot_present validation (Round 2 issue #1)
# Missing, null, string, number, and true must all fail at T0.
# ===================================================================
{
    my $x = raw();
    delete $x->{samples}[0]{graph_slot_present};
    my ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail',
        'T0 graph_slot_present missing fails' );

    $x = raw();
    $x->{samples}[0]{graph_slot_present} = undef;
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail',
        'T0 graph_slot_present null fails' );

    $x = raw();
    $x->{samples}[0]{graph_slot_present} = "false";
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail',
        'T0 graph_slot_present string fails' );

    $x = raw();
    $x->{samples}[0]{graph_slot_present} = 0;
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail',
        'T0 graph_slot_present number (0) fails' );

    $x = raw();
    $x->{samples}[0]{graph_slot_present} = JSON::PP::true;
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail',
        'T0 graph_slot_present true fails' );

    $x = raw();
    $x->{samples}[0]{graph_generation_id} = 'g';
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail',
        'T0 graph_generation_id non-null fails' );

    $x = raw();
    delete $x->{samples}[0]{graph_generation_id};
    my ( $missing, undef, $missing_rc ) = run_eval($x);
    is( $missing->{candidate}{status}, 'fail',
        'T0 graph_generation_id missing fails' );
    isnt( $missing_rc, 0,
        'T0 graph_generation_id missing exits nonzero' );
    like( enc( $missing->{candidate}{checks} ),
        qr/samples\[0\]\.graph_generation_id is required at T0/,
        'T0 graph_generation_id missing identifies the required signal' );

    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{samples}[0]{graph_generation_id} = undef;
    my ( $null, undef, $null_rc ) = run_eval($x);
    is( $null->{candidate}{status}, 'pass',
        'T0 graph_generation_id explicit JSON null remains valid' );
    is( $null_rc, 0,
        'T0 graph_generation_id explicit JSON null exits 0' );
}

# ===================================================================
# 4. Numeric gate boundary pairs: equality passes, just-over fails
#    (Round 2 issue #2)
# ===================================================================

# --- server_peak ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    $base->{samples}[1]{server_rss_bytes} = 3584 * $M;
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass', 'server_peak equality at 3.5 GiB passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[1]{server_rss_bytes} = 3584 * $M + 1;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail', 'server_peak just over 3.5 GiB fails' );
}

# --- warm_job_peak ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    $base->{samples}[1]{warm_job_rss_bytes} = 3584 * $M;
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass', 'warm_job_peak equality at 3.5 GiB passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[1]{warm_job_rss_bytes} = 3584 * $M + 1;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail', 'warm_job_peak just over 3.5 GiB fails' );
}

# --- route_rss_delta ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes} = 1 + 32 * $M;
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass', 'route_rss_delta equality at 32 MiB passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes} = 1 + 32 * $M + 1;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail', 'route_rss_delta just over 32 MiB fails' );
}

# --- board_pass_duration ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    $base->{board_passes}[0]{duration_ms} = 120000;
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass',
        'board_pass_duration equality at 120000 ms passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{board_passes}[0]{duration_ms} = 120001;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail',
        'board_pass_duration just over 120000 ms fails' );
}

# --- oom_delta ---
{
    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[5]{cgroup_oom_kill_count} = 1;
    my ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail', 'oom_delta just over 0 fails' );
}

# --- restart_delta ---
{
    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[5]{restart_count} = 1;
    my ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail', 'restart_delta just over 0 fails' );
}

# --- t2_jemalloc_retained_delta ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    # T1 retained 512 MiB, T2 retained 768 MiB -> delta = 256 MiB
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass',
        't2_jemalloc_retained_delta equality at 256 MiB passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[5]{jemalloc_retained_bytes} = 768 * $M + 1;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail',
        't2_jemalloc_retained_delta just over 256 MiB fails' );
}

# ===================================================================
# 5. T2 RSS delta: both branches of max(128 MiB, 10% of T1)
# ===================================================================

# --- 128 MiB branch: T1 low enough that 10% < 128 MiB ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    # T1 = 1024 MiB; 10% = 102 MiB < 128 MiB; threshold = 128 MiB
    $base->{samples}[5]{server_rss_bytes} = 1024 * $M + 128 * $M;
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass',
        '128 MiB branch of T2 RSS: equality passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[5]{server_rss_bytes} = 1024 * $M + 128 * $M + 1;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail',
        '128 MiB branch of T2 RSS: just over fails' );
}

# --- 10% branch: T1 high enough that 10% > 128 MiB ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    $base->{samples}[2]{server_rss_bytes} = 2048 * $M;
    $base->{samples}[5]{server_rss_bytes} = 2048 * $M + int( 2048 * $M / 10 );
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass',
        '10% branch of T2 RSS: equality passes' );

    my $over = raw();
    $over->{route_samples}[0]{rss_after_bytes}--;
    $over->{samples}[2]{server_rss_bytes} = 2048 * $M;
    $over->{samples}[5]{server_rss_bytes} = 2048 * $M + int( 2048 * $M / 10 ) + 1;
    ($a) = run_eval($over);
    is( $a->{candidate}{status}, 'fail',
        '10% branch of T2 RSS: just over fails' );
}

# --- 10% vs 128 MiB crossover: T1 = 1280 MiB, 10% = 128 MiB ---
{
    my $base = raw();
    $base->{route_samples}[0]{rss_after_bytes}--;
    $base->{samples}[2]{server_rss_bytes} = 1280 * $M;
    $base->{samples}[5]{server_rss_bytes} = 1280 * $M + 128 * $M;
    my ($a) = run_eval($base);
    is( $a->{candidate}{status}, 'pass',
        '10% vs 128 MiB crossover: equality at 1280 MiB T1 passes' );
}

# ===================================================================
# 6. Same-generation stability and drift
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{samples}[5]{graph_generation_id} = 'drift';
    my ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'generation drift at T2 fails' );

    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{samples}[1]{graph_generation_id} = 'other';
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'generation drift at graph_install fails' );
}

# ===================================================================
# 7. Malformed evidence must always fail (Round 1 issues #3, #5)
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{samples}[0]{server_rss_bytes} = '1';
    my ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'numeric string for server_rss_bytes fails' );

    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{route_samples}[0]{http_status} = '200';
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'numeric string for http_status fails' );

    # All numeric fields encoded as JSON strings.
    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    for my $s ( @{ $x->{samples} } ) {
        for my $k (qw(
            timestamp_unix_ms server_rss_bytes warm_job_rss_bytes
            process_anon_rss_bytes cgroup_current_bytes cgroup_oom_kill_count
            jemalloc_allocated_bytes jemalloc_resident_bytes
            jemalloc_retained_bytes graph_slot_approx_serialized_bytes
            graph_slot_node_count graph_slot_edge_count restart_count
          ) )
        {
            $s->{$k} = "$s->{$k}";
        }
    }
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'all numeric fields as JSON strings fails' );
}

# ===================================================================
# 8. Unsupported / forward schema version rejection
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{schema} = 'memory-sustainability-raw/v2';
    my ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'forward schema version v2 fails' );

    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{schema} = 'unknown-schema/v1';
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'unknown schema version fails' );
}

# ===================================================================
# 9. Malformed wrapper and missing phases
# ===================================================================
{
    my ($bad) = run_eval( [] );
    is( $bad->{candidate}{status}, 'fail', 'array wrapper root emits failure report' );

    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    pop @{ $x->{samples} };
    my ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'missing T2 phase fails' );

    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    splice @{ $x->{samples} }, 3, 2;
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'missing burst phase fails' );

    $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{samples}[1]{run_id} = 'other-run';
    ($a) = run_eval($x);
    is( $a->{candidate}{status}, 'fail', 'mixed run_id in samples fails' );
}

# ===================================================================
# 10. Pre-change diagnostic: cannot mask candidate, renders full details
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    $x->{samples}[1]{server_rss_bytes} = 3584 * $M + 1;
    my $d = raw();
    $d->{route_samples}[0]{rss_after_bytes}--;
    my ( $a, $m ) = run_eval( $x, $d );
    is( $a->{candidate}{status}, 'fail', 'diagnostic cannot mask candidate failure' );
    ok( exists $a->{pre_change_diagnostic}, 'diagnostic section present in JSON' );
    is( $a->{pre_change_diagnostic}{status}, 'pass', 'diagnostic passes independently' );

    like( $m, qr/Pre-change-image diagnostic/, 'diagnostic section in Markdown' );
    like( $m, qr/server_peak.*\|.*bytes/s, 'diagnostic server_peak with units renders' );
    like( $m, qr/warm_job_peak.*\|.*bytes/s, 'diagnostic warm_job_peak with units renders' );
    like( $m, qr/cannot change the candidate release status/i,
        'diagnostic non-gating label renders' );
}

# ===================================================================
# 11. Check structure completeness
# ===================================================================
{
    my $x = raw();
    $x->{route_samples}[0]{rss_after_bytes}--;
    my ($a) = run_eval($x);
    for my $chk ( @{ $a->{candidate}{checks} } ) {
        ok( defined $chk->{observed}, "$chk->{name} has observed" );
        ok( defined $chk->{threshold}, "$chk->{name} has threshold" );
        ok( defined $chk->{units}, "$chk->{name} has units" );
        ok( ref $chk->{evidence} eq 'ARRAY' && @{ $chk->{evidence} },
            "$chk->{name} has non-empty evidence array" );
        ok( grep { $chk->{status} eq $_ } qw(pass fail error),
            "$chk->{name} has valid status" );
    }
}

done_testing();
