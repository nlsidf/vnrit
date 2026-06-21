#define _GNU_SOURCE
#include <gst/gst.h>
#include <dlfcn.h>
#include <string.h>
#include <stdlib.h>

#define PACKAGE "mcenc"

typedef struct AMediaCodec AMediaCodec;
typedef struct AMediaFormat AMediaFormat;
typedef struct ANativeWindow ANativeWindow;

static struct {
    void *handle;
    AMediaCodec*(*createEncoderByType)(const char*);
    int (*delete)(AMediaCodec*);
    int (*configure)(AMediaCodec*,const AMediaFormat*,ANativeWindow*,void*,unsigned int);
    int (*start)(AMediaCodec*);
    int (*stop)(AMediaCodec*);
    long (*dequeueInputBuffer)(AMediaCodec*,long long);
    unsigned char*(*getInputBuffer)(AMediaCodec*,size_t,size_t*);
    int (*queueInputBuffer)(AMediaCodec*,size_t,size_t,size_t,unsigned long long,unsigned int);
    long (*dequeueOutputBuffer)(AMediaCodec*,void*,long long);
    unsigned char*(*getOutputBuffer)(AMediaCodec*,size_t,size_t*);
    int (*releaseOutputBuffer)(AMediaCodec*,size_t,int);
    AMediaFormat*(*getOutputFormat)(AMediaCodec*);
    AMediaFormat*((*aNew))(void);
    void ((*aDel))(AMediaFormat*);
    void (*setString)(AMediaFormat*,const char*,const char*);
    void (*setInt32)(AMediaFormat*,const char*,int);
    int (*getInt32)(AMediaFormat*,const char*,int*);
    int (*getBuffer)(AMediaFormat*,const char*,void**,size_t*);
    int ok;
} mc;

#define LOAD(n) do{mc.n=dlsym(mc.handle,"AMediaCodec_"#n);if(!mc.n){g_critical("dlsym AMediaCodec_"#n" failed");goto fail;}}while(0)
#define LOADF(n,s) do{mc.n=dlsym(mc.handle,"AMediaFormat_"#s);if(!mc.n){g_critical("dlsym AMediaFormat_"#s" failed");goto fail;}}while(0)

static void load_mediandk(void)
{
    if(mc.ok)return;
    mc.handle=dlopen("/system/lib64/libmediandk.so",RTLD_LAZY|RTLD_LOCAL);
    if(!mc.handle)mc.handle=dlopen("libmediandk.so",RTLD_LAZY|RTLD_LOCAL);
    if(!mc.handle){g_critical("libmediandk.so not found");return;}
    LOAD(createEncoderByType);LOAD(delete);LOAD(configure);LOAD(start);LOAD(stop);
    LOAD(dequeueInputBuffer);LOAD(getInputBuffer);LOAD(queueInputBuffer);
    LOAD(dequeueOutputBuffer);LOAD(getOutputBuffer);LOAD(releaseOutputBuffer);
    LOAD(getOutputFormat);
    LOADF(aNew,new);LOADF(aDel,delete);LOADF(setString,setString);LOADF(setInt32,setInt32);
    LOADF(getInt32,getInt32);LOADF(getBuffer,getBuffer);
    mc.ok=1;return;
fail:mc.ok=-1;
}

typedef struct{unsigned int offset,size;long long presentationTimeUs;unsigned int flags;}McBufInfo;
enum{MC_TRY_AGAIN=-1,MC_FORMAT_CHANGED=-2,MC_CONFIGURE_ENCODE=1,MC_COLOR_NV12=21};

typedef struct _GstMcEnc GstMcEnc;
typedef struct _GstMcEncClass GstMcEncClass;
struct _GstMcEnc{
    GstElement   parent;
    GstPad      *sinkpad,*srcpad;
    int          bitrate,framerate,width,height,negotiated;
    AMediaCodec *codec;
    int          codec_running,csd_ready,csd_size;
    unsigned char csd_buf[256];
    GstCaps      *out_caps;
    unsigned char *carry_buf;
    int            carry_cap,carry_size,has_carry;
    long long      carry_pts;
};
struct _GstMcEncClass{GstElementClass parent_class;};
static GType gst_mc_enc_get_type(void);
#define GST_TYPE_MC_ENC (gst_mc_enc_get_type())
#define GST_MC_ENC(obj) ((GstMcEnc*)(obj))
G_DEFINE_TYPE(GstMcEnc,gst_mc_enc,GST_TYPE_ELEMENT)
enum{PROP_0,PROP_BITRATE,PROP_FRAMERATE};

static GstStaticPadTemplate sink_tmpl=GST_STATIC_PAD_TEMPLATE(
    "sink",GST_PAD_SINK,GST_PAD_ALWAYS,
    GST_STATIC_CAPS("video/x-raw,format=NV12,width=[1,4096],height=[1,4096],framerate=[0/1,2147483647/1]"));
static GstStaticPadTemplate src_tmpl=GST_STATIC_PAD_TEMPLATE(
    "src",GST_PAD_SRC,GST_PAD_ALWAYS,
    GST_STATIC_CAPS("video/x-h264,stream-format=byte-stream,alignment=au,width=[1,4096],height=[1,4096],framerate=[0/1,2147483647/1]"));

static void stop_codec(GstMcEnc *s);
static void set_out_caps(GstMcEnc *s)
{
    if(s->out_caps){gst_caps_unref(s->out_caps);}
    s->out_caps=gst_caps_new_simple("video/x-h264","stream-format",G_TYPE_STRING,"byte-stream",
        "alignment",G_TYPE_STRING,"au","width",G_TYPE_INT,s->width,"height",G_TYPE_INT,s->height,
        "framerate",GST_TYPE_FRACTION,s->framerate,1,NULL);
    gst_pad_set_caps(s->srcpad,s->out_caps);
}
static int start_codec(GstMcEnc *s)
{
    if(s->codec)stop_codec(s);
    load_mediandk();
    if(mc.ok<=0){g_warning("mcenc: mediandk not available");return -1;}
    s->codec=mc.createEncoderByType("video/avc");
    if(!s->codec){g_warning("mcenc: create encoder failed");return -1;}
    void *fmt=mc.aNew();
    mc.setString(fmt,"mime","video/avc");
    mc.setInt32(fmt,"width",s->width);mc.setInt32(fmt,"height",s->height);
    mc.setInt32(fmt,"bitrate",s->bitrate*1000);
    mc.setInt32(fmt,"frame-rate",s->framerate);
    mc.setInt32(fmt,"i-frame-interval",10);
    mc.setInt32(fmt,"color-format",MC_COLOR_NV12);
    mc.setInt32(fmt,"stride",s->width);mc.setInt32(fmt,"slice-height",s->height);
    mc.setInt32(fmt,"latency",1);mc.setInt32(fmt,"push-blank-buffers-on-stop",1);
    int st=mc.configure(s->codec,fmt,NULL,NULL,MC_CONFIGURE_ENCODE);
    mc.aDel(fmt);
    if(st){g_warning("mcenc: configure failed: %d",st);goto fail;}
    st=mc.start(s->codec);
    if(st){g_warning("mcenc: start failed: %d",st);goto fail;}
    s->codec_running=1;s->csd_ready=0;s->csd_size=0;s->has_carry=0;s->carry_size=0;
    set_out_caps(s);
    g_message("mcenc: started %dx%d %dkbps %dfps",s->width,s->height,s->bitrate,s->framerate);
    return 0;
fail:mc.delete(s->codec);s->codec=NULL;return -1;
}
static void stop_codec(GstMcEnc *s)
{
    if(!s->codec)return;
    if(s->codec_running){mc.stop(s->codec);s->codec_running=0;}
    mc.delete(s->codec);s->codec=NULL;s->negotiated=0;
}
static void handle_fmt(GstMcEnc *s)
{
    void *fmt=mc.getOutputFormat(s->codec);if(!fmt)return;
    int w,h;if(mc.getInt32(fmt,"width",&w)&&mc.getInt32(fmt,"height",&h)){s->width=w;s->height=h;}
    s->csd_size=0;
    void *b=NULL;size_t sz=0;
    if(mc.getBuffer(fmt,"csd-0",&b,&sz)&&b&&sz>0){
        int sp=s->csd_size,rem=(int)sizeof(s->csd_buf)-sp,add=(int)sz;
        if(add>rem)add=rem;
        memcpy(s->csd_buf+sp,b,add);s->csd_size+=add;
    }
    if(mc.getBuffer(fmt,"csd-1",&b,&sz)&&b&&sz>0){
        int sp=s->csd_size,rem=(int)sizeof(s->csd_buf)-sp,add=(int)sz;
        if(add>rem)add=rem;
        memcpy(s->csd_buf+sp,b,add);s->csd_size+=add;
    }
    mc.aDel(fmt);
    set_out_caps(s);
    if(s->csd_size>0)s->csd_ready=1;
}
static int submit(GstMcEnc *s,const unsigned char *data,int size,long long pts)
{
    long idx=mc.dequeueInputBuffer(s->codec,10000);
    if(idx<0)return(idx==MC_TRY_AGAIN)?0:-1;
    size_t bs;unsigned char*buf=mc.getInputBuffer(s->codec,(size_t)idx,&bs);
    if(!buf)return -1;
    if((size_t)size>bs)size=(int)bs;
    memcpy(buf,data,size);
    return mc.queueInputBuffer(s->codec,(size_t)idx,0,(size_t)size,(unsigned long long)pts,0)?-1:0;
}
static int drain_one(GstMcEnc *s,unsigned char *out,int cap,long long *out_pts)
{
    McBufInfo info;long idx=mc.dequeueOutputBuffer(s->codec,&info,10000);
    if(idx==MC_TRY_AGAIN)return 0;
    if(idx==MC_FORMAT_CHANGED){handle_fmt(s);return -2;}
    if(idx<0){g_warning("mcenc: dequeueOutputBuffer: %ld",idx);return -1;}
    size_t bs;unsigned char*buf=mc.getOutputBuffer(s->codec,(size_t)idx,&bs);
    if(!buf){mc.releaseOutputBuffer(s->codec,(size_t)idx,0);return -1;}
    int wr=0;
    if(s->csd_ready&&s->csd_size>0){
        int c=s->csd_size;if(c>cap)c=cap;
        memcpy(out,s->csd_buf,c);wr+=c;s->csd_ready=0;
    }
    int ds=(int)info.size;if(wr+ds>cap)ds=cap-wr;
    if(ds>0)memcpy(out+wr,buf+info.offset,ds);wr+=ds;
    if(out_pts)*out_pts=info.presentationTimeUs;
    mc.releaseOutputBuffer(s->codec,(size_t)idx,0);
    return wr;
}
static void drain_all(GstMcEnc *s)
{
    s->carry_size=0;s->carry_pts=-1;
    while(s->carry_size<s->carry_cap){
        long long pts=-1;int n=drain_one(s,s->carry_buf+s->carry_size,s->carry_cap-s->carry_size,&pts);
        if(n==-2)continue;if(n<=0)break;
        if(s->carry_pts<0)s->carry_pts=pts;s->carry_size+=n;
    }
    s->has_carry=s->carry_size>0;
}
static GstFlowReturn gst_mc_enc_chain(GstPad *pad,GstObject *parent,GstBuffer *buf)
{
    GstMcEnc*s=GST_MC_ENC(parent);
    if(!s->codec||!s->codec_running){gst_buffer_unref(buf);return GST_FLOW_NOT_NEGOTIATED;}
    GstMapInfo map;
    if(!gst_buffer_map(buf,&map,GST_MAP_READ)){gst_buffer_unref(buf);return GST_FLOW_ERROR;}
    long long pts=(GST_BUFFER_PTS(buf)!=GST_CLOCK_TIME_NONE)?(long long)(GST_BUFFER_PTS(buf)/1000):-1;
    submit(s,map.data,(int)map.size,pts);
    gst_buffer_unmap(buf,&map);gst_buffer_unref(buf);
    drain_all(s);if(!s->has_carry)return GST_FLOW_OK;
    if(!s->out_caps){g_message("mcenc: no out_caps");return GST_FLOW_OK;}
    GstBuffer*out=gst_buffer_new_allocate(NULL,s->carry_size,NULL);
    gst_buffer_fill(out,0,s->carry_buf,s->carry_size);
    if(s->carry_pts>=0)GST_BUFFER_PTS(out)=(GstClockTime)s->carry_pts*1000;
    GST_BUFFER_DTS(out)=GST_BUFFER_PTS(out);
    s->has_carry=0;s->carry_size=0;
    return gst_pad_push(s->srcpad,out);
}
static gboolean gst_mc_enc_sink_event(GstPad*pad,GstObject*parent,GstEvent*event)
{
    GstMcEnc*s=GST_MC_ENC(parent);
    if(GST_EVENT_TYPE(event)==GST_EVENT_CAPS){
        GstCaps*caps;gst_event_parse_caps(event,&caps);
        GstStructure*str=gst_caps_get_structure(caps,0);
        gst_structure_get_int(str,"width",&s->width);
        gst_structure_get_int(str,"height",&s->height);
        int fn,fd;if(gst_structure_get_fraction(str,"framerate",&fn,&fd)&&fd>0)s->framerate=fn/fd;
        if(!s->codec&&start_codec(s)){gst_event_unref(event);return FALSE;}
        s->negotiated=1;
    }
    return gst_pad_event_default(pad,parent,event);
}
static gboolean gst_mc_enc_src_query(GstPad*pad,GstObject*parent,GstQuery*query)
{
    GstMcEnc*s=GST_MC_ENC(parent);
    if(GST_QUERY_TYPE(query)==GST_QUERY_CAPS){
        GstCaps*filter=NULL;gst_query_parse_caps(query,&filter);
        GstCaps*caps=gst_caps_new_simple("video/x-h264","stream-format",G_TYPE_STRING,"byte-stream",
            "alignment",G_TYPE_STRING,"au","width",GST_TYPE_INT_RANGE,1,4096,
            "height",GST_TYPE_INT_RANGE,1,4096,"framerate",GST_TYPE_FRACTION_RANGE,0,1,2147483647,1,NULL);
        if(s->width>0&&s->height>0){GstCaps*fix=gst_caps_copy(caps);
            gst_structure_fixate_field_nearest_int(gst_caps_get_structure(fix,0),"width",s->width);
            gst_structure_fixate_field_nearest_int(gst_caps_get_structure(fix,0),"height",s->height);
            gst_caps_unref(caps);caps=fix;}
        if(filter){GstCaps*i=gst_caps_intersect(caps,filter);gst_caps_unref(caps);caps=i;}
        gst_query_set_caps_result(query,caps);gst_caps_unref(caps);return TRUE;
    }
    return gst_pad_query_default(pad,parent,query);
}
static gboolean gst_mc_enc_sink_query(GstPad*pad,GstObject*parent,GstQuery*query)
{
    if(GST_QUERY_TYPE(query)==GST_QUERY_ACCEPT_CAPS){
        GstCaps*caps=NULL;gst_query_parse_accept_caps(query,&caps);
        if(caps){int accept=0;
            for(unsigned i=0;i<gst_caps_get_size(caps)&&!accept;i++){
                const char*f=gst_structure_get_string(gst_caps_get_structure(caps,i),"format");
                if(f&&!g_strcmp0(f,"NV12"))accept=1;}
            gst_query_set_accept_caps_result(query,accept);return TRUE;}
    }
    return gst_pad_query_default(pad,parent,query);
}
static GstStateChangeReturn gst_mc_enc_change_state(GstElement*el,GstStateChange tr)
{
    GstMcEnc*s=GST_MC_ENC(el);
    if(tr==GST_STATE_CHANGE_PAUSED_TO_READY)stop_codec(s);
    return GST_ELEMENT_CLASS(gst_mc_enc_parent_class)->change_state(el,tr);
}
static void gst_mc_enc_set_prop(GObject*o,unsigned id,const GValue*v,GParamSpec*ps)
{
    GstMcEnc*s=GST_MC_ENC(o);
    if(id==PROP_BITRATE)s->bitrate=g_value_get_int(v);
    else if(id==PROP_FRAMERATE)s->framerate=g_value_get_int(v);
}
static void gst_mc_enc_get_prop(GObject*o,unsigned id,GValue*v,GParamSpec*ps)
{
    GstMcEnc*s=GST_MC_ENC(o);
    if(id==PROP_BITRATE)g_value_set_int(v,s->bitrate);
    else if(id==PROP_FRAMERATE)g_value_set_int(v,s->framerate);
}
static void gst_mc_enc_finalize(GObject*o)
{
    GstMcEnc*s=GST_MC_ENC(o);stop_codec(s);
    if(s->out_caps){gst_caps_unref(s->out_caps);s->out_caps=NULL;}
    if(s->carry_buf){free(s->carry_buf);s->carry_buf=NULL;}
    G_OBJECT_CLASS(gst_mc_enc_parent_class)->finalize(o);
}
static void gst_mc_enc_class_init(GstMcEncClass*klass)
{
    GObjectClass*goc=G_OBJECT_CLASS(klass);GstElementClass*gec=GST_ELEMENT_CLASS(klass);
    goc->set_property=gst_mc_enc_set_prop;goc->get_property=gst_mc_enc_get_prop;
    goc->finalize=gst_mc_enc_finalize;
    g_object_class_install_property(goc,PROP_BITRATE,g_param_spec_int("bitrate","Bitrate","Bitrate in kbps",1,100000,5000,G_PARAM_READWRITE|G_PARAM_STATIC_STRINGS));
    g_object_class_install_property(goc,PROP_FRAMERATE,g_param_spec_int("framerate","Framerate","Encoder framerate",1,120,30,G_PARAM_READWRITE|G_PARAM_STATIC_STRINGS));
    gec->change_state=gst_mc_enc_change_state;
    gst_element_class_set_static_metadata(gec,"MediaCodec H.264 Encoder","Codec/Encoder/Video","Hardware H.264 via NDK AMediaCodec","opencode");
    gst_element_class_add_static_pad_template(gec,&sink_tmpl);gst_element_class_add_static_pad_template(gec,&src_tmpl);
}
static void gst_mc_enc_init(GstMcEnc*s)
{
    s->bitrate=5000;s->framerate=30;s->width=0;s->height=0;s->negotiated=0;
    s->codec=NULL;s->codec_running=0;s->csd_ready=0;s->csd_size=0;
    s->out_caps=NULL;
    s->carry_cap=524288;s->carry_buf=(unsigned char*)malloc(s->carry_cap);
    s->carry_size=0;s->has_carry=0;
    s->sinkpad=gst_pad_new_from_static_template(&sink_tmpl,"sink");
    gst_pad_set_event_function(s->sinkpad,gst_mc_enc_sink_event);
    gst_pad_set_chain_function(s->sinkpad,gst_mc_enc_chain);
    gst_pad_set_query_function(s->sinkpad,gst_mc_enc_sink_query);
    gst_element_add_pad(GST_ELEMENT(s),s->sinkpad);
    s->srcpad=gst_pad_new_from_static_template(&src_tmpl,"src");
    gst_pad_set_query_function(s->srcpad,gst_mc_enc_src_query);
    gst_element_add_pad(GST_ELEMENT(s),s->srcpad);
}
static gboolean plugin_init(GstPlugin*plugin)
{
    GType type=gst_mc_enc_get_type();
    return gst_element_register(plugin,"mcenc",GST_RANK_PRIMARY+100,type);
}
GST_PLUGIN_DEFINE(GST_VERSION_MAJOR,GST_VERSION_MINOR,mcenc,
    "MediaCodec H.264 encoder via NDK AMediaCodec",
    plugin_init,"1.0","LGPL",PACKAGE,"https://opencode.ai")
