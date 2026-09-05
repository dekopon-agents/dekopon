/* Study-only Linux static supervisor, no allocator injection or signal-handler work.
 * Container cgroup is the hard process-tree owner. All values are bytes/ns/counts.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <gnu/libc-version.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static volatile sig_atomic_t stopped;
static uint64_t beginning;
/* PID1 exit is the kernel-owned whole-namespace termination barrier, even if the
 * Python owner dies, sampling blocks, or descendants ignore signals / call setsid. */
static void deadline(int s) { (void)s; _exit(124); }
static const char *phases[]={"load","retained","clones","clones-dropped","dropped","throughput"};
static int synchronize(int input,int output,pid_t pid,int *index,const char **phase);

static void stop(int s) { (void)s; stopped=1; }
static uint64_t ns(void) { struct timespec t; if(clock_gettime(CLOCK_MONOTONIC,&t)) exit(71); return (uint64_t)t.tv_sec*1000000000ULL+t.tv_nsec; }
static void pause_ms(long ms) { struct timespec t={ms/1000,(ms%1000)*1000000}; nanosleep(&t,NULL); }
static int number(const char *path, unsigned long long *value) { FILE *f=fopen(path,"r"); if(!f) return 0; int ok=fscanf(f,"%llu",value)==1; fclose(f); return ok; }
static void metric(const char *key,unsigned long long val) { printf(",\"%s\":%llu",key,val); }
static int preflight(void) {
    unsigned long long memory=0,swap=1,pids=0,quota=0,period=0;
    struct statvfs d; struct utsname u;
    FILE *f=fopen("/sys/fs/cgroup/cpu.max","r");
    if(f) { if(fscanf(f,"%llu %llu",&quota,&period)!=2) quota=0; fclose(f); }
    if(!number("/sys/fs/cgroup/memory.max",&memory)||!number("/sys/fs/cgroup/memory.swap.max",&swap)||!number("/sys/fs/cgroup/pids.max",&pids)||memory!=1073741824ULL||swap!=0||pids!=64||quota==0||quota!=period||statvfs("/work/run",&d)||uname(&u)) {
        fprintf(stderr,"unsafe-prerequisite: cgroup v2 memory/swap/pids/cpu verification\n"); return 78;
    }
    unsigned long long free_bytes=(unsigned long long)d.f_bavail*d.f_frsize;
    if(free_bytes<8ULL*1024*1024*1024) { fprintf(stderr,"resource-exhaustion: Docker disk below 8GiB\n"); return 78; }
    printf("{\"kind\":\"environment\",\"os\":\"Linux\",\"architecture\":\"%s\",\"page_bytes\":%ld,\"collector_libc\":\"static-glibc-%s\",\"docker_disk_free_bytes\":%llu,\"memory_max\":%llu,\"memory_swap_max\":%llu,\"pids_max\":%llu,\"cpu_quota\":%llu,\"cpu_period\":%llu}\n",u.machine,sysconf(_SC_PAGESIZE),gnu_get_libc_version(),free_bytes,memory,swap,pids,quota,period);
    return 0;
}
static void status(pid_t pid,int observer) {
    char path[80],line[1024],key[80]; unsigned long long val;
    snprintf(path,sizeof(path),"/proc/%d/status",pid); FILE *f=fopen(path,"r");
    if(!f) return;
    while(fgets(line,sizeof(line),f)) if(sscanf(line,"%79s %llu",key,&val)==2) {
        if(!strcmp(key,"VmRSS:")) metric(observer?"collector_rss_bytes":"process_rss_bytes",val*1024);
        if(!observer&&!strcmp(key,"VmSize:")) metric("process_virtual_bytes",val*1024);
        if(!observer&&!strcmp(key,"VmHWM:")) metric("process_hwm_bytes",val*1024);
    }
    fclose(f);
}
static void mappings(pid_t pid) {
    char path[80],line[1024],key[80]; unsigned long long val;
    snprintf(path,sizeof(path),"/proc/%d/smaps_rollup",pid); FILE *f=fopen(path,"r");
    if(f) {
        while(fgets(line,sizeof(line),f)) {
            if(sscanf(line,"%79s %llu",key,&val)==2) {
                if(!strcmp(key,"Pss:")) metric("process_pss_bytes",val*1024);
                if(!strcmp(key,"Anonymous:")) metric("anonymous_rss_bytes",val*1024);
            }
        }
        fclose(f);
    }
    snprintf(path,sizeof(path),"/proc/%d/smaps",pid); f=fopen(path,"r");
    if(!f) return;
    unsigned long long jit=0; int selected=0,lines=0;
    while(fgets(line,sizeof(line),f)&&++lines<200000) {
        unsigned long a,b,offset,inode; unsigned int major,minor; char perms[5]; int end=0;
        if(sscanf(line,"%lx-%lx %4s %lx %x:%x %lu %n",&a,&b,perms,&offset,&major,&minor,&inode,&end)==7) {
            char *name=line+end; while(*name==' '||*name=='\t') name++;
            selected=perms[2]=='x' && (*name=='\n'||*name=='\0');
        } else if(selected && sscanf(line,"Rss: %llu kB",&val)==1) jit+=val*1024;
    }
    fclose(f);
    if(lines<200000) metric("anonymous_executable_rss_bytes",jit);
}
static void io(pid_t pid) {
    char path[80],line[128],key[64]; unsigned long long val;
    snprintf(path,sizeof(path),"/proc/%d/io",pid); FILE *f=fopen(path,"r");
    if(!f) return;
    while(fgets(line,sizeof(line),f)) {
        if(sscanf(line,"%63s %llu",key,&val)==2) {
            if(!strcmp(key,"read_bytes:")) metric("read_bytes",val);
            if(!strcmp(key,"write_bytes:")) metric("write_bytes",val);
        }
    }
    fclose(f);
}
static void sample(pid_t pid,const char *phase,int deep) {
    unsigned long long current=0,peak=0,inactive=0,val; char key[80];
    printf("{\"phase\":\"%s\",\"clock_origin\":\"observer-relative\",\"elapsed_ns\":%llu,\"metrics\":{\"wall_time_ns\":%llu",phase,(unsigned long long)(ns()-beginning),(unsigned long long)(ns()-beginning));
    status(pid,0); status(getpid(),1); if(deep) mappings(pid); io(pid);
    int present=number("/sys/fs/cgroup/memory.current",&current);
    if(present) metric("cgroup_memory_current_bytes",current);
    if(number("/sys/fs/cgroup/memory.peak",&peak)) metric("cgroup_memory_peak_bytes",peak);
    FILE *f=fopen("/sys/fs/cgroup/memory.stat","r");
    if(f) { while(fscanf(f,"%79s %llu",key,&val)==2) {
        if(!strcmp(key,"anon")) metric("cgroup_anon_bytes",val);
        if(!strcmp(key,"file")) metric("cgroup_file_bytes",val);
        if(!strcmp(key,"kernel")) metric("cgroup_kernel_bytes",val);
        if(!strcmp(key,"inactive_file")) inactive=val;
    } fclose(f); if(present) metric("cgroup_working_set_bytes",current>inactive?current-inactive:0); }
    puts("}}");
}
/* A single outstanding phase byte and acknowledgement: the child cannot cross a
 * barrier during a memory snapshot. Counters and child timings keep separate clocks. */
static int synchronize(int input,int output,pid_t pid,int *index,const char **phase) {
    unsigned char next; ssize_t n=read(input,&next,1);
    if(n<0) return (errno==EAGAIN||errno==EINTR)?0:-1;
    if(n==0) return 0;
    if((*index==3||*index==4) && next==(unsigned char)(128+*index) && *phase) {
        printf("{\"kind\":\"phase-transition\",\"protocol\":\"history-pipe-v1\",\"phase\":\"%s\"}\n",phases[*index]);
        *phase=NULL; /* No sample can straddle the destructive operation. */
        return write(output,&next,1)==1?1:-1;
    }
    if(*index>=6 || next!=(unsigned char)*index || ((*index==3||*index==4)&&*phase)) return -1;
    *phase=phases[(*index)++];
    printf("{\"kind\":\"phase-sync\",\"protocol\":\"history-pipe-v1\",\"phase\":\"%s\",\"elapsed_ns\":%llu}\n",*phase,(unsigned long long)(ns()-beginning));
    sample(pid,*phase,1);
    return write(output,&next,1)==1?1:-1;
}
int main(int argc,char **argv) {
    if(argc<4 || strcmp(argv[1],"--deadline") || getpid()!=1) return 78;
    char *end=NULL; long seconds=strtol(argv[2],&end,10);
    if(!end || *end || seconds<1 || seconds>180) return 78;
    struct sigaction action={0}; action.sa_handler=deadline; sigemptyset(&action.sa_mask);
    if(sigaction(SIGALRM,&action,NULL)) return 71;
    alarm((unsigned)seconds); /* Installed before preflight or any child launch. */
    argc-=2; argv+=2;
    setvbuf(stdout,NULL,_IOLBF,0); beginning=ns(); signal(SIGTERM,stop); signal(SIGINT,stop);
    int checked=preflight(); if(checked) return checked;
    if(argc==2&&!strcmp(argv[1],"--self-test")) { sample(getpid(),"instrumentation",1); puts("{\"kind\":\"complete\",\"exit_code\":0}"); return 0; }
    if(argc==2&&!strcmp(argv[1],"--deadline-self-test")) {
        pid_t child=fork(); if(child<0) return 71;
        if(child==0) {
            if(setsid()<0) _exit(71);
            signal(SIGINT,SIG_IGN); signal(SIGTERM,SIG_IGN);
            puts("study-noncooperative-descendant-started");
            for(;;) pause();
        }
        /* Deliberately non-polling PID1; only its independent alarm can terminate it. */
        for(;;) pause();
    }
    int broker=argc>2&&!strcmp(argv[1],"--broker");
    if(argc<3||(!broker&&strcmp(argv[1],"--child"))) { fprintf(stderr,"usage: collector --broker|--child executable args\n"); return 64; }
    int markers[2]={-1,-1},acks[2]={-1,-1};
    if(!broker && (pipe(markers)||pipe(acks))) return 71;
    pid_t pid=fork(); if(pid<0) return 71;
    if(pid==0) {
        if(!broker) {
            if(dup2(markers[1],100)<0 || dup2(acks[0],101)<0) _exit(71);
            close(markers[0]); close(markers[1]); close(acks[0]); close(acks[1]);
        }
        execv(argv[2],argv+2); perror("execv"); _exit(127);
    }
    if(!broker) {
        close(markers[1]); close(acks[0]);
        if(fcntl(markers[0],F_SETFL,O_NONBLOCK)<0) return 71;
    }
    int phase_index=0; const char *phase="startup";
    unsigned long long ready=0,shutdown=0; int status_code=0,done=0; struct rusage usage;
    int checkpoints[]={0,1,10,20,30,60},next=0;
    while(!done) {
        pid_t r=wait4(pid,&status_code,WNOHANG,&usage);
        if(r==pid) { done=1; break; }
        if(r<0) { perror("wait4"); return 71; }
        uint64_t elapsed=ns()-beginning;
        if(broker&&!ready&&access("/work/run/broker.sock",F_OK)==0) {
            ready=elapsed;
            printf("{\"phase\":\"ready\",\"elapsed_ns\":%llu,\"metrics\":{\"ready_latency_ns\":%llu}}\n",ready,ready);
        }
        int deep=!broker;
        if(ready&&next<6&&elapsed-ready>=(uint64_t)checkpoints[next]*1000000000ULL) { deep=1; next++; }
        if(!broker) {
            int sync=synchronize(markers[0],acks[1],pid,&phase_index,&phase);
            if(sync<0) { fprintf(stderr,"invalid History phase handshake\n"); return 78; }
            if(!sync && phase) sample(pid,phase,deep);
        } else sample(pid,ready?"idle":"startup",deep);
        if(!shutdown && (stopped || (broker&&ready&&elapsed-ready>=60000000000ULL) || elapsed>=160000000000ULL)) {
            shutdown=elapsed; if(kill(pid,SIGINT)&&errno!=ESRCH) { perror("kill"); return 71; }
        }
        if(shutdown&&elapsed-shutdown>5000000000ULL) { if(kill(pid,SIGKILL)&&errno!=ESRCH) { perror("kill"); return 71; } }
        pause_ms(broker?(ready?1000:100):20);
    }
    unsigned long long cpu=((unsigned long long)usage.ru_utime.tv_sec+usage.ru_stime.tv_sec)*1000000000ULL+((unsigned long long)usage.ru_utime.tv_usec+usage.ru_stime.tv_usec)*1000ULL;
    printf("{\"phase\":\"drained\",\"elapsed_ns\":%llu,\"metrics\":{\"cpu_time_ns\":%llu,\"minor_faults\":%ld,\"major_faults\":%ld}}\n",(unsigned long long)(ns()-beginning),cpu,usage.ru_minflt,usage.ru_majflt);
    int code=WIFEXITED(status_code)?WEXITSTATUS(status_code):128+WTERMSIG(status_code);
    if(stopped || (broker&&!ready) || (!broker&&phase_index!=6)) code=code?code:78;
    printf("{\"kind\":\"complete\",\"exit_code\":%d}\n",code);
    return code;
}
