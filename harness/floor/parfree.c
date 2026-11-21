// GB of 4K-page anon memory touched by 8 threads; PARFREE=1: each thread munmaps its region in parallel before MARK
#include <time.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdio.h>
#include <pthread.h>
static size_t per; static char* reg[8];
static void* w(void*a){ long i=(long)a; char*p=mmap(0,per,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0); for(size_t o=0;o<per;o+=4096)p[o]=1; reg[i]=p; return 0;}
static int mode; static void* f(void*a){ long i=(long)a; if(mode==2) madvise(reg[i],per,MADV_DONTNEED); else munmap(reg[i],per); return 0;}
static double now(){struct timespec t; clock_gettime(CLOCK_REALTIME,&t); return t.tv_sec+t.tv_nsec*1e-9;}
int main(void){ double gb=atof(getenv("GB")?getenv("GB"):"4"); int T=8; per=(size_t)(gb*(1UL<<30))/T; pthread_t t[8];
 for(long i=0;i<T;i++)pthread_create(&t[i],0,w,(void*)i); for(int i=0;i<T;i++)pthread_join(t[i],0);
 double t0=now();
 if(getenv("PARFREE")){ mode=atoi(getenv("PARFREE")); for(long i=0;i<T;i++)pthread_create(&t[i],0,f,(void*)i); for(int i=0;i<T;i++)pthread_join(t[i],0);}
 double t1=now(); fprintf(stderr,"MARK %.9f parfree_ms %.0f\n",t1,(t1-t0)*1000); _exit(0);}
