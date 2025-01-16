CURDIR := $(shell pwd)
BASEDIR := $(abspath $(CURDIR)/..)

PROJECT ?= $(notdir $(BASEDIR))
PROJECT := $(strip $(PROJECT))

CAP_IMPL ?= generic

ifeq ($(MODE), debug)
	CFLAGS ?= -O0 -g
	CXXFLAGS ?= -O0 -g
else
	CFLAGS ?= -O3
	CXXFLAGS ?= -O3
endif

CUDA_LIBS ?= /opt/cuda/targets/x86_64-linux/lib
HIP_LIBS ?= /opt/rocm/lib/

C_SRC_DIR = $(CURDIR)
C_SRC_OUTPUT ?= $(CURDIR)/../priv/arweave.so
C_SRC_CAPACITY_OUTPUT ?= $(CURDIR)/../priv/capacity.so
TESTS_DIR = $(CURDIR)/tests
TESTS_OUTPUT = $(TESTS_DIR)/tests

# System type and C compiler/flags.

UNAME_SYS := $(shell uname -s)
ifeq ($(UNAME_SYS), Darwin)
	OSX_CPU_ARCH ?= x86_64
	# nix systems may not have sysctl where uname -m will return the correct arch
	SYSCTL_EXISTS := $(shell which sysctl 2>/dev/null)
	ifdef SYSCTL_EXISTS
		ifneq ($(shell sysctl -n machdep.cpu.brand_string | egrep "M(1|2)"),)
	  	OSX_CPU_ARCH = arm64
		endif
  else
		ifneq ($(shell uname -m | egrep "arm64"),)
	  	OSX_CPU_ARCH = arm64
	 	endif
  endif
	CC ?= cc
	CFLAGS += -std=c99 -arch $(OSX_CPU_ARCH) -finline-functions -Wall -Wmissing-prototypes
	CXXFLAGS += -arch $(OSX_CPU_ARCH) -finline-functions -Wall
	LDFLAGS ?= -arch $(OSX_CPU_ARCH)
	LDFLAGS += -flat_namespace -undefined suppress
else ifeq ($(UNAME_SYS), FreeBSD)
	CC ?= cc
	CFLAGS += -std=c99 -finline-functions -Wall -Wmissing-prototypes
	CXXFLAGS += -finline-functions -Wall
else ifeq ($(UNAME_SYS), Linux)
	CC ?= gcc
	CFLAGS += -std=c99 -finline-functions -Wall -Wmissing-prototypes
	CXXFLAGS += -finline-functions -Wall
endif

ifneq (, $(shell which pkg-config))
	CFLAGS += `pkg-config --cflags gmp openssl`
	CXXFLAGS += `pkg-config --cflags gmp openssl`
	LDFLAGS += `pkg-config --libs gmp openssl`
endif

CFLAGS += -fPIC -I /usr/local/include -Ofast -g0 -march=native
CXXFLAGS += -fPIC -std=c++17 -Ofast -g0 -march=native
CXXFLAGS_NVCC += -O3 -std=c++17 -Xcompiler -fPIC -DCAP_IMPL_CUDA -ccbin /usr/bin/gcc-13
CXXFLAGS_HIPCC += -O3 -std=c++17 -fPIC -DCAP_IMPL_HIP

LDLIBS += -L /usr/local/lib -lgmp -lssl -lcrypto
LDLIBS_CAP_LIB += -L $(CUDA_LIBS) -L $(HIP_LIBS) -L /usr/local/lib -lgmp -lssl -lcrypto

# Verbosity.

c_verbose_0 = @echo " C     " $(?F);
c_verbose = $(c_verbose_$(V))

cpp_verbose_0 = @echo " CPP   " $(?F);
cpp_verbose = $(cpp_verbose_$(V))

nvcc_verbose_0 = @echo " NVCC   " $(?F);
nvcc_verbose = $(nvcc_verbose_$(V))

hipcc_verbose_0 = @echo " HIPCC   " $(?F);
hipcc_verbose = $(hipcc_verbose_$(V))

link_verbose_0 = @echo " LD    " $(@F);
link_verbose = $(link_verbose_$(V))

ALL_LIB_SOURCES_W_CAPACITY := $(wildcard $(C_SRC_DIR)/*.c $(C_SRC_DIR)/*.cpp)
CAPACITY_SOURCES := $(wildcard $(C_SRC_DIR)/capacity_single.c )
ALL_LIB_SOURCES := $(filter-out $(CAPACITY_SOURCES), $(ALL_LIB_SOURCES_W_CAPACITY))
ifeq ($(CAP_IMPL), cuda)
	CAPACITY_SOURCES += $(wildcard $(C_SRC_DIR)/capacity_cuda.cu )
	LDLIBS_CAP_LIB += -lcuda -lcudart
	CFLAGS += -DCAP_IMPL_CUDA
	CXXFLAGS += -DCAP_IMPL_CUDA
endif
ifeq ($(CAP_IMPL), hip)
	CAPACITY_SOURCES += $(wildcard $(C_SRC_DIR)/capacity_hip.hip )
	LDLIBS_CAP_LIB += -lamdhip64
	CFLAGS += -DCAP_IMPL_HIP
	CXXFLAGS += -DCAP_IMPL_HIP
endif
C_SOURCES := $(filter-out $(ENIF_SOURCES), $(ALL_LIB_SOURCES))
TESTS_SOURCES = $(wildcard $(TESTS_DIR)/*.cpp)

ALL_LIB_OBJECTS = $(addsuffix .o, $(basename $(ALL_LIB_SOURCES)))
CAPACITY_LIB_OBJECTS = $(addsuffix .o, $(basename $(CAPACITY_SOURCES)))
ENIF_OBJECTS = $(addsuffix .o, $(basename $(ENIF_SOURCES)))
C_OBJECTS = $(addsuffix .o, $(basename $(C_SOURCES)))
TESTS_OBJECTS = $(addsuffix .o, $(basename $(TESTS_SOURCES)))

COMPILE_C = $(c_verbose) $(CC) $(CFLAGS) $(CPPFLAGS) -c
COMPILE_CPP = $(cpp_verbose) $(CXX) $(CXXFLAGS) $(CPPFLAGS) -c
COMPILE_NVCC = $(nvcc_verbose) nvcc $(CXXFLAGS_NVCC) $(CPPFLAGS) -c
COMPILE_HIPCC = $(hipcc_verbose) hipcc $(CXXFLAGS_HIPCC) $(CPPFLAGS) -c

$(C_SRC_CAPACITY_OUTPUT): $(CAPACITY_LIB_OBJECTS) $(C_SRC_OUTPUT)
	@mkdir -p $(BASEDIR)/priv/
	$(link_verbose) $(CC) $(CAPACITY_LIB_OBJECTS) $(LDFLAGS) $(LDLIBS_CAP_LIB) -shared -o $(C_SRC_CAPACITY_OUTPUT)

$(C_SRC_OUTPUT): $(ALL_LIB_OBJECTS)
	@mkdir -p $(BASEDIR)/priv/
	$(link_verbose) $(CXX) $(ALL_LIB_OBJECTS) $(LDFLAGS) $(LDLIBS) -shared -o $(C_SRC_OUTPUT)

%.o: %.c
	$(COMPILE_C) $(OUTPUT_OPTION) $<

%.o: %.cu
	$(COMPILE_NVCC) $(OUTPUT_OPTION) $<

%.o: %.hip
	$(COMPILE_HIPCC) $(OUTPUT_OPTION) $<

%.o: %.cc
	$(COMPILE_CPP) $(OUTPUT_OPTION) $<

%.o: %.C
	$(COMPILE_CPP) $(OUTPUT_OPTION) $<

%.o: %.cpp
	$(COMPILE_CPP) $(OUTPUT_OPTION) $<

$(TESTS_OUTPUT): $(C_OBJECTS) $(TESTS_OBJECTS)
	$(link_verbose) $(CXX) $(C_OBJECTS) $(TESTS_OBJECTS) $(LDFLAGS) $(LDLIBS) -o $(TESTS_OUTPUT)

tests: $(TESTS_OUTPUT)

all: $(C_SRC_CAPACITY_OUTPUT) $(C_SRC_OUTPUT)

clean:
	@rm -f $(C_SRC_OUTPUT) $(C_SRC_CAPACITY_OUTPUT) $(TESTS_OUTPUT) $(ALL_LIB_OBJECTS) $(CAPACITY_LIB_OBJECTS) $(TESTS_OBJECTS)

.PHONY : all



