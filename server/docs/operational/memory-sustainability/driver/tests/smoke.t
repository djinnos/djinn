use strict; use warnings; use Test::More;
use File::Temp qw(tempdir); use File::Spec; use JSON::PP;
my $root = File::Spec->rel2abs(File::Spec->catdir(File::Spec->curdir));
$root =~ s{/server/docs/operational/memory-sustainability/driver/tests\z}{};
my $driver = "$root/server/docs/operational/memory-sustainability/driver/memory_workload.pl";
my $dir=tempdir(CLEANUP=>1); my $out="$dir/smoke.jsonl";
my @cmd=('perl',$driver,'--fake','--profile','smoke','--output',$out,'--t0-seconds','0','--t1-seconds','0','--burst-seconds','0','--t2-seconds','0','--request-count','6');
is(system(@cmd),0,'fixture-backed fake state machine and production-format parsers succeed');
open my $fh,'<',$out or die $!; my @r=map {decode_json($_)} <$fh>; close $fh;
is($r[0]{defaults}{t0_seconds},1800,'production T0 default remains encoded');
is($r[0]{defaults}{burst_seconds},7200,'production burst default remains encoded');
is($r[0]{defaults}{t2_seconds},300,'production T2 delay default remains encoded');
is($r[0]{defaults}{board_tick_seconds},300,'production board cadence default remains encoded');
is($r[0]{defaults}{request_count},100,'production request-count default remains encoded');
my @requests=grep {$_->{kind} eq 'galaxy_request'} @r;
is_deeply([map {$_->{status}} @requests],[200,304,200,304,200,304],'requests alternate 200/304');
ok(!(grep { !defined $_->{rss_before_bytes} || !defined $_->{rss_after_bytes} } @requests),
   'every route request retains its collected before/after RSS evidence');
ok(scalar(grep {$_->{kind} eq 'preflight' && $_->{fixture_observed}{seed} eq 'ste6-smoke-v1'} @r),'preflight observes fixture identity before install');
ok(scalar(grep {$_->{kind} eq 'oom_delta'} @r),'OOM baseline and delta are preserved');
ok(scalar(grep {$_->{kind} eq 'peak' && $_->{warm_peak_bytes}} @r),'warm and server peaks are recorded');
ok(!-e "$out.partial",'successful evidence finalized atomically');
my $timed="$dir/timed.jsonl";
my @timed_cmd=('perl',$driver,'--fake','--profile','smoke','--output',$timed,'--t0-seconds','0','--t1-seconds','0','--burst-seconds','0.4','--t2-seconds','0.15','--board-tick-seconds','0.1','--request-count','5');
is(system(@timed_cmd),0,'short overrides exercise the production timing scheduler');
open $fh,'<',$timed or die $!; my @timed=map {decode_json($_)} <$fh>; close $fh;
my @timed_requests=grep {$_->{kind} eq 'galaxy_request'} @timed;
is(scalar @timed_requests,5,'all configured requests complete');
ok($timed_requests[0]{scheduled_offset_ms}<1 && $timed_requests[-1]{scheduled_offset_ms}>=399,
   'request targets span the configured burst');
ok($timed_requests[-1]{request_completed_offset_ms}>=390 &&
   $timed_requests[-1]{request_started_offset_ms}-$timed_requests[0]{request_started_offset_ms}>=390,
   'requests execute throughout the burst and the final request completes at its end');
my @timed_boards=grep {$_->{kind} eq 'board_pass'} @timed;
ok(@timed_boards>=4 && $timed_boards[-1]{scheduled_offset_ms}>=300,
   'independent board cadence remains represented throughout the burst');
my ($t2_timing)=grep {$_->{kind} eq 't2_timing'} @timed;
ok($t2_timing && $t2_timing->{configured_delay_ms}==150 && $t2_timing->{actual_delay_ms}>=150,
   'T2 delay starts at actual final-request completion');
is($t2_timing->{final_request_completed_unix_ms},$timed_requests[-1]{request_completed_unix_ms},
   'T2 timing evidence identifies the actual final request completion');
my $bad="$dir/malformed.jsonl";
{ local $ENV{DJINN_FAKE_MALFORMED}='events';
  isnt(system('perl',$driver,'--fake','--profile','smoke','--output',$bad,'--t0-seconds','0','--t1-seconds','0','--burst-seconds','0','--t2-seconds','0','--request-count','1'),0,'malformed cgroup events fail preflight');
}
ok(-e "$bad.partial",'malformed preflight preserves partial raw evidence');
open $fh,'<',"$bad.partial" or die $!; my @bad=map {decode_json($_)} <$fh>; close $fh;
ok($bad[-1]{kind} eq 'run_finalized' && !$bad[-1]{success},'failed partial evidence is finalized as unsuccessful');
my $restarted="$dir/restarted.jsonl";
{ local $ENV{DJINN_FAKE_RESTARTS}='0,1';
  isnt(system('perl',$driver,'--fake','--profile','smoke','--output',$restarted,'--t0-seconds','0','--t1-seconds','0','--burst-seconds','0','--t2-seconds','0','--request-count','1'),0,'changed restart counter fails the run');
}
ok(-e "$restarted.partial",'restart failure preserves partial evidence');
open $fh,'<',"$restarted.partial" or die $!; my @restarted=map {decode_json($_)} <$fh>; close $fh;
my ($restart_delta)=grep {$_->{kind} eq 'restart_delta'} @restarted;
ok($restart_delta && $restart_delta->{baseline}==0 && $restart_delta->{current}==1 && $restart_delta->{delta}==1,'restart delta is recorded before failure');
my $interrupted="$dir/interrupted.jsonl";
my $pid=fork(); die "fork: $!" unless defined $pid;
if(!$pid){ exec 'perl',$driver,'--fake','--profile','smoke','--output',$interrupted,'--t0-seconds','5','--t1-seconds','0','--burst-seconds','0','--t2-seconds','0','--request-count','1'; die "exec: $!"; }
sleep 1; kill 'TERM',$pid; waitpid($pid,0);
is($? >> 8,130,'SIGTERM returns interrupted status');
ok(-e "$interrupted.partial",'interruption preserves partial evidence');
open $fh,'<',"$interrupted.partial" or die $!; my @int=map {decode_json($_)} <$fh>; close $fh;
is($int[-2]{kind},'interrupted','interruption record precedes finalization');
my $invalid="$dir/invalid.jsonl";
isnt(system('perl',$driver,'--fake','--profile','smoke','--output',$invalid,'--t0-seconds','0','--t1-seconds','0','--burst-seconds','0','--t2-seconds','0','--request-count','0'),0,'invalid override fails before workload');
done_testing;
